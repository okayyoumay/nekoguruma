# ADR-058: NumReceiveCycles = 0 Means "No Response Required," Not "Wait for One"

**Date:** 2026-07-04
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/events.rs` (`wait_for_expected_response`,
             `handle_send_recv`), `j2534-0404-service/src/service/rpc_primitive.rs`
             (`rpc_start_com_primitive`)

## Context

ADR-053 introduced `PDU_COP_CTRL_DATA.NumReceiveCycles` handling and documented
`0` as "one matching response (legacy single-shot behaviour)" — i.e. the
service treated an unset/zero field the same as `1`. `handle_send_recv`
additionally gated the entire receive phase on `expected_response_array` being
non-empty: an empty array skipped `wait_for_expected_response` outright and
finished the COP immediately after the write, regardless of
`NumReceiveCycles`.

While specifying `CP_P3Func`/`CP_P3Phys` (the minimum inter-request gap
enforced when the *preceding* transmission on a shared bus required no
response), the caller defined the trigger condition as "`receiveCycle=0`" —
i.e. `NumReceiveCycles == 0` on that prior send is the spec's own signal for
"this transmission needs no response at all." That is irreconcilable with
ADR-053's "0 = wait for one response": under the old code, `NumReceiveCycles
== 0` could never mean "no response," since it was silently upgraded to `1`.

The caller confirmed directly that ADR-053's own implementation of
`NumReceiveCycles` was the thing that needed to change, not the P3
description, and clarified the intended semantics precisely:

- A caller that wants exactly one matching response passes `NumReceiveCycles
  = 1` explicitly (not `0`, not an unset/default field).
- A caller that wants `n` matching responses passes `n`, exactly as `n > 0`
  already worked.
- `expected_response_array` being empty is not a special case orthogonal to
  `NumReceiveCycles`. If `NumReceiveCycles > 0` and the array is empty,
  nothing can ever match, so the natural (and correct) outcome is a receive
  timeout — the same outcome as a non-empty descriptor that never matches.

No test in this codebase exercised `NumReceiveCycles == 0` against a
non-empty `expected_response_array` before this ADR, so the conflict between
ADR-053's text and the P3 spec's "receiveCycle=0" condition went unnoticed
until specified directly.

## Decision

### `NumReceiveCycles == 0` skips the receive phase entirely

`wait_for_expected_response`'s `matches_needed` computation drops the special
case that mapped `0` to `Some(1)`; `0` now falls through to `Some(0)` like any
other exact count. A new check right after computing `matches_needed` returns
`ReceivePhaseOutcome::CycleComplete` immediately, before entering the poll
loop at all, whenever `matches_needed == Some(0)`:

```rust
let matches_needed: Option<u32> = match num_receive_cycles {
    -2 | -1 => None,
    n => Some(n as u32), // 0 => no response required
};
if matches_needed == Some(0) {
    return ReceivePhaseOutcome::CycleComplete;
}
```

This is a genuine "no receive phase at all" — no RX polling, no
`CP_P2Max`/RC-handling snapshot consulted, no `PduErrEvtRxTimeout` possible —
matching the send-cycle accounting that follows unconditionally in
`handle_send_recv`.

### The fire-and-forget decision no longer depends on `expected_response_array`

`handle_send_recv` previously only entered the receive phase at all when
`expected_response` was non-empty; an empty array bypassed
`wait_for_expected_response` unconditionally. That gate is removed:
`wait_for_expected_response` is now always called, and it alone decides
whether to skip the wait, based purely on `NumReceiveCycles`. An empty
`expected_response_array` with `NumReceiveCycles > 0` now enters the poll
loop like any other call, can never match, and ends in a receive timeout when
the `CP_P2Max` window closes — exactly as a non-empty descriptor that never
matches would.

### Existing tests relying on the old `0 = wait for one` behaviour

Every test that set `num_receive_cycles: 0` alongside a non-empty
`expected_response_array` and expected to wait for a match now sets it to `1`
explicitly (`response_distribution.rs`, `rc_handling.rs`, `lifecycle.rs`).
Call sites that set `num_receive_cycles: 0` with an *empty*
`expected_response_array` (`harness.rs::send_data`,
`locks_and_param_classes.rs`) needed no change: that combination was, and
remains, fire-and-forget under both the old and new semantics.

## Consequences

- A client that wants a single matching response must now pass
  `NumReceiveCycles = 1`. A client that omits the field (proto default `0`,
  same as explicitly passing `0`) gets a fire-and-forget send with no receive
  phase — this is a real behaviour change from ADR-053 for any caller that
  relied on the unset-field default to mean "wait for one response."
- `CP_P3Func`/`CP_P3Phys`'s "receiveCycle=0" trigger condition (the preceding
  functional/physical-addressed transmission on the shared bus required no
  response) now has a well-defined, implementable meaning:
  `NumReceiveCycles == 0` on that prior send. Implementing the actual
  minimum-inter-request-gap enforcement itself is tracked separately and not
  part of this ADR.
- `tests/grpc_mock/cop_ctrl_cycles.rs` gained two tests:
  `sendrecv_zero_receive_cycles_completes_without_waiting_for_response` (a
  long `CP_P2Max` plus a non-empty `expected_response_array` with no RX ever
  injected — the COP must still finish almost immediately) and
  `sendrecv_empty_expected_response_with_nonzero_cycles_times_out` (an empty
  array with `NumReceiveCycles = 1` and a short `CP_P2Max` — the COP must
  finish via `PduErrEvtRxTimeout`, not silently). Both were verified to fail
  against the pre-fix code and pass against the fix.
- ADR-053's "Receive phase (`NumReceiveCycles`)" section and ADR-006's
  empty-array fire-and-forget description are amended in place to point here.
