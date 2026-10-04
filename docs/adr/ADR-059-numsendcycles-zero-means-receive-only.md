# ADR-059: NumSendCycles = 0 Means "No Send at All, Receive-Only"

**Date:** 2026-07-04
**Status:** Accepted (extended by ADR-100 — tier-2 Receive Only list role)
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`),
             `j2534-0404-service/src/service/events.rs` (`handle_send_recv`)

## Context

ADR-053 introduced `PDU_COP_CTRL_DATA.NumSendCycles` handling and normalised
`0` to a single send (`send_cycles_remaining = if num_send_cycles == 0 { 1 }
else { num_send_cycles }`), on the grounds that a caller leaving the field
unset should still get the previous one-shot request/response behaviour.

The caller specified the intended semantics directly: `NumSendCycles == 0`
must mean the primitive performs no transmission at all — a receive-only
capture with no corresponding request — mirroring how ADR-058 already
redefined `NumReceiveCycles == 0` as "no response required" rather than
"wait for one." `NumSendCycles == 0` together with `NumReceiveCycles == 0`
means neither a send nor a receive phase happens; the COP still performs
whatever other state changes it would otherwise perform (in particular,
`temp_param_update`'s hardware ComParam apply/revert, ISO 22900-2 §9.4.3 —
a real state change independent of whether any data goes out, analogous to
the comm-state changes `CoptStartcomm` itself performs without any
send/receive of application data) and still emits its normal
`PduCopstExecuting`/`PduCopstFinished` status transitions.

This mirrors ADR-058's change to `NumReceiveCycles` and completes the same
correction on the send side: both cycle-count fields now use `0` uniformly
to mean "this phase doesn't happen," rather than one of them silently
defaulting to "do it once."

## Decision

### `send_cycles_remaining` is passed through unchanged

`rpc_start_com_primitive` no longer normalises `num_send_cycles == 0` to `1`:

```rust
let send_cycles_remaining = num_send_cycles;
```

`0` and `-1` continue to be valid (validation already accepted `>= -1`); `0`
now reaches `handle_send_recv` as a real, distinct value instead of being
silently rewritten before the `TxItem::SendRecv` is even queued.

### `handle_send_recv` skips the write, not the side effects

A new `should_transmit = send_cycles_remaining != 0` gate wraps only the
`transmit_request` call (the actual `PassThruWriteMsgs`/software-ISO-TP send):

```rust
let write_ok = if !should_transmit {
    true
} else if temp_params_ok {
    match transmit_request(...) { ... }
} else {
    false
};
```

`temp_param_update`'s apply-Working/revert-to-Active hardware calls remain
outside this gate and run exactly as before regardless of `should_transmit` —
they are a state change this COP performs independent of whether it
transmits, not a side effect of the write itself. Likewise, the
`PduCopstExecuting`/`PduCopstFinished` status emissions and the receive
phase (`wait_for_expected_response`, ADR-058) are both already unconditional
of the write outcome and needed no change.

### Cycle accounting: `0` stays `0`, not `-1`

The post-cycle bookkeeping decremented `send_cycles_remaining` by one to
compute the next cycle's remaining count. Naively applying that to `0` would
produce `-1` — indistinguishable from the *infinite* cyclic-send sentinel.
`0` is special-cased to stay `0`:

```rust
let remaining = match send_cycles_remaining {
    -1 => -1,
    0 => 0,
    n => n - 1,
};
```

Since `remaining == 0` always ends the COP after the current cycle, a
`NumSendCycles = 0` primitive is inherently a single, non-repeating pass:
there is nothing to send again, so there is no concept of scheduling a
follow-up send cycle. `Time`/cyclic-send scheduling is simply unused in this
case, exactly as it already is for a plain one-shot `NumSendCycles = 1`.

### Interaction with `NumReceiveCycles`

The two fields are orthogonal, as already established by ADR-058:

- `NumSendCycles = 0`, `NumReceiveCycles = n > 0` (or `-1`/`-2`) — receive-only:
  no request is transmitted, but the receive phase runs exactly as it would
  after a real send, matching against whatever arrives unsolicited.
- `NumSendCycles = 0`, `NumReceiveCycles = 0` — neither phase runs; the COP
  completes immediately (after any `temp_param_update` side effect) with
  only its status transitions to show for it.
- `NumSendCycles > 0` (or `-1`), `NumReceiveCycles = 0` — already covered by
  ADR-058: send happens, no response is awaited (fire-and-forget).

RC21/23/78 pending-response auto-handling (ADR-018) re-sends the *original*
request data when triggered mid-receive-phase. This re-request logic is
unaffected by this ADR and unchanged: it is unlikely to trigger meaningfully
in a `NumSendCycles = 0` receive-only capture (there was no original request
for an ECU to be pending against), but nothing prevents it from firing if an
incoming frame happens to carry one of those NRC bytes at the configured
`CP_RCByteOffset`. This edge case is not addressed here.

### Existing call sites depending on the old normalisation

Every test that previously relied on `NumSendCycles` being unset/`0` and
expecting a real single transmission now sets it to `1` explicitly:
`tests/grpc_mock/harness.rs::send_data` (the shared fire-and-forget-style
helper used across `can_mode.rs`, `comparam_tx.rs`, and others),
`lifecycle.rs`, `rc_handling.rs`, `response_distribution.rs`, and two
`can_mode.rs` tests that previously passed `cop_ctrl_data: None` (proto
default `0` for an absent/unset field) and expected a single send.

## Consequences

- **This is a breaking behaviour change for any caller that omits
  `PDU_COP_CTRL_DATA` or leaves `num_send_cycles` unset expecting a normal
  single send.** Proto3 has no field-presence distinction for a plain
  `int32`, so an absent `cop_ctrl_data` and an explicit `num_send_cycles: 0`
  are indistinguishable at this layer and now both mean "receive-only, no
  transmission." Callers that want the previous default single-send
  behaviour must now set `num_send_cycles: 1` explicitly — the same
  opt-in-explicit requirement ADR-058 already imposed on `NumReceiveCycles`
  for "wait for one response." This is a deliberate symmetry with ADR-058,
  not an oversight, but it means every existing client of this service must
  audit its `StartComPrimitive(COPT_SENDRECV)` call sites for an
  omitted/zero `num_send_cycles` that relied on the old default.
- A receive-only `CoptSendrecv` (`NumSendCycles = 0`) is otherwise a normal
  COP: it gets a `cop_handle`, emits `PduCopstExecuting`/`PduCopstFinished`,
  honours `CancelComPrimitive`, and its receive phase is governed by
  `NumReceiveCycles`/`CP_P2Max` exactly like any other cycle's.
  `temp_param_update` continues to stage/revert hardware ComParams even when
  nothing is transmitted.
- `tests/grpc_mock/cop_ctrl_cycles.rs` gained two tests:
  `sendrecv_zero_send_cycles_skips_transmission_and_only_receives` (asserts
  zero writes reach the mock, then an unsolicited matching frame still
  completes the COP) and `sendrecv_zero_send_and_zero_receive_cycles_does_neither`
  (asserts zero writes and a prompt `PduCopstFinished` with neither phase
  running). Both were verified to fail against the pre-fix code (which
  still transmitted once) and pass against the fix.
- ADR-053's `NumSendCycles` description is amended in place to point here.
