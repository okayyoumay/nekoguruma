# ADR-111: CoptStartcomm's Optional Message Is Transmitted, Not Discarded

**Date:** 2026-07-22
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs`, `src/service/rpc_primitive.rs`,
`src/service/events.rs`

## Context

`j2534-0404-service/docs/iso22900-2-conformance-audit.md` finding A1-4:
`StartComPrimitive(COPT_STARTCOMM, ...)` on a CAN/J1850 (non-K-line) link
silently discarded a non-empty `cop_data` instead of transmitting it. ISO
22900-2:2009 §9.2.6.3.2 b) says, for a CoptStartcomm on a
protocol that does not require an initialization sequence: when `pCopData`
carries bytes, the D-PDU API must transmit them as a request message; when
`NumReceiveCycles` is nonzero it must also wait for the response(s); and in
both cases the ComLogicalLink moves to `PDU_CLLST_COMM_STARTED` once the
transmission (and reception, if any) has finished.

Table 7 (ComPrimitive/CLL-state cross-reference) step 5 pairs `CoptStartcomm`
with the `PDU_CLLST_COMM_STARTED` transition unconditionally — the same
pairing case d) uses for the fast-init request, describing it as "a normal
SendRecv ComPrimitive" for timeout-handling purposes. Prior to this fix,
`handle_start_comm` only ever consulted `cop_data` for K-line (ISO9141/
ISO14230) five-baud/fast-init dispatch; a non-K-line link's `cop_data` was
read only for a `warn!`-and-discard diagnostic, and its `expected_response_array`/
`NumReceiveCycles` were never even parsed on this path — a data-loss bug of
the same shape ADR-085/ADR-087 already fixed for `CoptStopcomm`'s analogous
final message.

## Decision

Reuse `CoptStopcomm`'s existing one-shot transmit(+receive) machinery
(ADR-085/ADR-087) for the new CAN/J1850 CoptStartcomm optional-message case,
rather than inventing a parallel mechanism. The shared struct — previously
`StopCommTx` — is renamed `OneShotCommTx` to reflect the new second caller;
this is a pure rename, `CoptStopcomm`'s own behavior is unchanged byte-for-byte.
`rpc_start_com_primitive`'s `CoptStartcomm` branch resolves an
`Option<OneShotCommTx>` eagerly (ADR-067 call-time-binding discipline,
unchanged): `None` when `cop_data` is empty or the link is K-line (mutually
exclusive with `five_baud`/`fast_init`, `Some` only for K-line); `Some`
otherwise, via the same `resolve_send_recv_tx` pipeline `CoptSendrecv`/
`CoptStopcomm` use. `TxItem::StartComm`'s dead `cop_data: Vec<u8>` field
(kept, pre-fix, only for the now-deleted warn-and-discard diagnostic) is
replaced by `tx: Option<OneShotCommTx>`. `handle_start_comm` gains a new
`else if let Some(tx) = tx` sibling branch to the existing
`five_baud.is_some() || fast_init.is_some()` branch — the two are
call-time-exclusive by construction, so exactly one (or neither, for
empty-`cop_data`/K-line-skip-init cases) ever runs per COP.

Two deliberate deviations from `CoptStopcomm`'s reuse of the same machinery:

1. **Resolved against `binding.resolved()`, not always-Active.**
   `CoptStopcomm`'s `temp_param_update` is a documented no-op (ADR-044/
   ADR-067) — its final message always resolves from the bound Active
   snapshot. `CoptStartcomm`'s `Temp` binding is genuinely pushed to
   hardware for the duration of the whole init/optional-message
   transaction (ADR-067 claims A/C), so the optional message's
   transmit+receive resolution (`SendRecvTx`, `RcHandlingConfig`,
   `response_timeout_ms`) must use `binding.resolved()` (Working when
   `temp_param_update` was set, Active otherwise) to stay consistent with
   the five-baud/fast-init resolution right above it in the same RPC
   branch, which already does this.
2. **`cancellable: true` throughout** (`wait_for_p3_gap`, `transmit_request`,
   and `wait_for_expected_response`'s `ExpectedResponseWait`), unlike
   `CoptStopcomm`'s `cancellable: false`. `CoptStopcomm`'s receive phase is
   non-cancellable because it runs strictly after `stop_comm_pending`'s
   state transition has already committed (ADR-087 Decision point 2) — by
   the time that phase runs, the stop is irreversible and the COP must
   always reach the terminal `Online`/`Finished` block. `CoptStartcomm`'s
   optional-message phase runs BEFORE anything has committed or changed CLL
   state — `comm_started` is still `false`, nothing has been torn down —
   so there is no analogous "point of no return" argument; a
   `CancelComPrimitive` arriving here can be honored exactly like any other
   pre-commit guard already in `handle_start_comm` (Guard A/A2/A3).

**Timeout/error semantics**, both spec-grounded:

- **RX timeout is non-fatal.** If the optional message transmits
  successfully but `NumReceiveCycles != 0` and no matching response arrives,
  `wait_for_expected_response` emits `PduErrEvtRxTimeout` (as it already does
  for every other caller) and returns `CycleComplete` — `handle_start_comm`
  falls through to the shared tail unconditionally, still reaching
  `PDU_CLLST_COMM_STARTED` + `PduCopstFinished`. This follows directly from
  §9.2.6.3.2 b)'s unconditional state-change sentence (quoted above — no
  timeout carve-out) and case d)'s explicit equation of the with-request
  StartComm to "a normal SendRecv ComPrimitive", whose own RX timeout
  likewise does not fail the COP (ADR-058).
- **TX failure is fatal, but distinct from an init failure.** If the
  transmit itself fails (`TxFailure::Event`/`ChannelLost`), the optional
  message never reached the bus at all, so the CLL must NOT transition to
  `PDU_CLLST_COMM_STARTED` — `handle_start_comm`'s new branch emits
  `PduCopstFinished` (after reverting hardware first, when `binding` is
  `Temp`) without ever emitting `PduCllstCommStarted`. Critically, no
  `PduErrEvtInitError` is emitted anywhere on this path: that event is
  specific to the K-line `run_protocol_init` sequence (five-baud/fast-init),
  which this optional-message path never runs — the transmit failure instead
  surfaces via the same `PduErrorEvent` `transmit_request`/`isotp_send`
  already produce for any other failed send (e.g. `PduErrEvtTxError`,
  `PduErrEvtRxTimeout` for an ISO-TP N_Bs timeout).
- **`NumReceiveCycles == -1` (IS-CYCLIC) is rejected synchronously**
  (`INVALID_ARGUMENT`) by `rpc_start_com_primitive`, mirroring
  `CoptStopcomm`'s identical rejection (ADR-087) — an "until cancelled"
  receive can never let the CLL reach a terminal state (`COMM_STARTED`
  here, `ONLINE` there).

A cancellation mid-receive-phase is handled by `wait_for_expected_response`
itself (it already emits `PduCopstCancelled` for the cancelled case, and the
guarded first-wins `PduCopstCancelled` for the disconnect/stale case, per its
existing per-pass check) — `handle_start_comm`'s new branch adds only the one
obligation that function knows nothing about: a `Temp` binding's hardware
revert (`revert_hardware_to_live_active`) before every one of its own
early-return paths (pre-transmit P3-gap cancellation, post-transmit
staleness, the transmit failure/cancellation arms, and the post-receive
`Terminal` outcome) — mirroring the existing K-line `Err` arm's identical
revert-before-return obligation a few lines above it in the same function.

## Consequences

- `TxItem::StartComm`'s `cop_data: Vec<u8>` field and the `!cop_data.is_empty()
  && !protocol_requires_init(protocol_id)` warn-and-discard block are both
  removed; `protocol_requires_init` (events.rs) had no remaining callers
  anywhere in the workspace after this and was deleted (confirmed via
  `cargo build`'s dead-code warning before removal).
- `StopCommTx` is renamed `OneShotCommTx` (service.rs, rpc_primitive.rs,
  events.rs) — a pure rename; `CoptStopcomm`'s own resolution, transmit, and
  receive-phase behavior are unchanged.
- One pre-existing test (`startcomm_comparam.rs`'s
  `can_explicit_five_baud_setting_is_inert_on_non_k_line_link`) previously
  relied on a non-empty `cop_data` with no CAN addressing configured being
  silently ignored on CAN; it now legitimately fails synchronously
  (`INVALID_ARGUMENT`, unresolvable addressing) under this fix, since that
  `cop_data` is no longer discarded. Updated to use empty `cop_data`,
  preserving its original, narrower intent (5-baud settings are inert on a
  non-K-line link) — coverage for the new transmit path lives in the new
  `startcomm_optional_message_tx.rs` test file.
- Accepted residual: a client relying on the old (spec-nonconformant)
  discard-and-proceed behavior for a non-empty `cop_data` on CAN/J1850 will
  now see that message actually reach the bus, and — if it configured no
  addressing (no `SetUniqueRespIdTable` entry) — will now get a synchronous
  `INVALID_ARGUMENT` at `StartComPrimitive` call time instead of a silent
  no-op. This is the intended, spec-conformant behavior change this ADR
  exists to make.
- Accepted residual (verification-pass finding): the new `else if let
  Some(tx) = tx` branch's `ReceivePhaseOutcome::Terminal` arm reverts a
  `Temp` binding's hardware unconditionally, unlike `handle_send_recv`'s own
  `!channel_lost`-gated revert for the structurally same situation.
  `ReceivePhaseOutcome` does not expose to its caller which of `Terminal`'s
  several causes (mid-receive cancellation, disconnect/stale, or a hard
  channel error) applied — unlike `transmit_request`'s `TxFailure::
  ChannelLost`, which this same branch's earlier `Err` arm DOES check
  explicitly, because that signal is local to the branch rather than folded
  into an opaque outcome enum returned by a shared helper. Every
  hard-error-caused `Terminal` here is confirmed (by reading
  `wait_for_expected_response_inner`) to be reachable only after
  `handle_channel_hard_error` has already run for this same channel, so the
  extra revert in that sub-case is a real but harmless `SET_CONFIG` call
  against an already-erroring channel (`apply_params_to_hardware` swallows
  its own failure) — wasteful, not unsafe. A post-hoc `still_on_this_channel`
  check cannot distinguish that sub-case from the non-hard-error stale/
  reconnect sub-case also folded into `Terminal`, which legitimately still
  needs the revert (consistent with every other `!still_on_this_channel`
  guard in this same branch); narrowing the guard incorrectly on the strength
  of that check would silently break the stale sub-case instead of fixing the
  hard-error one. Deliberately left as documented-safe-as-is rather than
  widening `ReceivePhaseOutcome`'s contract for every caller (`handle_send_
  recv`, `handle_stop_comm`) to expose the missing signal — this crate's
  mock harness has no hard-error-injection primitive anywhere in the test
  suite to prove a narrower guard correct, and a production change here
  without one would risk introducing a new, unverified bug. See the code
  comment at `events.rs`'s `ReceivePhaseOutcome::Terminal` arm inside
  `handle_start_comm`'s `else if let Some(tx) = tx` branch, and `j2534-0404-
  service/docs/implementation-notes.md`'s A1-4/ADR-111 backlog entry for the
  matching test-coverage residual.

See ADR-085/ADR-087 for the shared one-shot transmit/receive machinery's own
history and rationale, and ADR-067 for the call-time ComParam-binding
discipline (`ParamBinding`/`binding.resolved()`) this decision extends to the
optional-message resolution.
