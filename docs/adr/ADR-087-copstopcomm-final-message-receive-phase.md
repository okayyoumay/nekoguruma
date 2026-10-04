# ADR-087: CoptStopcomm's Final Message Gains a Bounded, Non-Cancellable Receive Phase

**Date:** 2026-07-15
**Status:** Accepted (amends ADR-085; amended by ADR-100)
**Affects:** `j2534-0404-service/src/service.rs`, `src/service/rpc_primitive.rs`,
`src/service/events.rs`

*Amends ADR-085: supersedes its "Alternatives Considered" #1 (the rejection of
a post-transmit response wait) only. Every other ADR-085 decision — the
transmit itself, its ordering, the non-cancellable transmit step (round 6),
the `stop_comm_pending` machinery and its six reset points, and the
`connect_generation` guards (round 7 / ADR-086) — stands unchanged, so
ADR-085's Status is not changed; an `*Update (ADR-087)*` note is added under
its Alternatives #1, following the precedent ADR-086 set in the same file.*

## Context

ADR-085 made `StartComPrimitive(COPT_STOPCOMM, cop_data=<non-empty>)` transmit
`cop_data` as a fire-and-forget final message, rejecting a response wait on
the ground that "`CoptStopcomm` has no `expected_response_array` or
`NumReceiveCycles` to define what 'a response' would even mean". That premise
is wrong: `PDU_COP_CTRL_DATA` (`iso22900-sys/src/bindings/d_pdu_api_defs.h`,
`NumPossibleExpectedResponses`/`pExpectedResponseArray`) and the proto's
`ComPrimitiveCtrlData` are generic across COP types — a client can, and
protocol reality requires it be able to, expect a response to its final
message (e.g. KWP2000 `StopCommunication` gets a positive response 0xC2
before the session ends). The adapter silently discarded those fields for
`CoptStopcomm`, so the ECU's answer was delivered only as unattributed
background RX traffic: no `acceptance_id`, no `cop_handle` attribution, no
`ResultData` event correlation — the same class of data-loss-by-omission
ADR-085 itself fixed for the request direction.

## Decision

When `cop_data` is non-empty, the `CoptStopcomm` branch now parses
`expected_response_array` and `NumReceiveCycles` exactly as `CoptSendrecv`
does, and `handle_stop_comm` runs one receive phase between the (unchanged,
non-cancellable) transmit and the (unchanged) terminal
`PduCllstOnline`/`PduCopstFinished` block, via the same
`wait_for_expected_response` engine — identical matching, `CP_P2Max` window,
RC21/23/78 handling, and the same `poll_rx_inner` delivery path, so matched
responses get `acceptance_id`/`cop_handle` attribution, an `rx_buf` entry,
and a `resultitem` (`ResultData`) notification bit-for-bit like
`CoptSendrecv` (ADR-085 Alternative #5's no-drift reasoning applies to the
receive engine the same way it applied to `resolve_send_recv_tx`).

Deviations from `CoptSendrecv`'s receive phase, each deliberate:

1. **`NumReceiveCycles = -1` (IS-CYCLIC) is a synchronous
   `INVALID_ARGUMENT`** (precedent: the 5-baud branch's own narrowing,
   ADR-076). An "until cancelled" receive contradicts a COP that must
   terminate to return the CLL to `PDU_CLLST_ONLINE`, and — combined with
   point 2 — would be an un-exitable wait that wedges the channel's FIFO
   poll task. `0` (fire-and-forget, the ADR-085 status quo and still the
   default when `cop_ctrl_data` is absent), `n > 0` (exact count), and `-2`
   (IS-MULTIPLE — the natural shape for a functionally-addressed stop
   answered by several ECUs) are all accepted. `< -2` is rejected with
   `CoptSendrecv`'s existing message. `NumSendCycles`/`Time` remain ignored
   (StopComm is inherently one-shot); all validation happens after the
   `stop_comm_pending` test-and-set and therefore uses ADR-085's existing
   rollback exits.
2. **The receive phase is non-cancellable**, extending round 6's rule from
   the transmit to the whole post-teardown tail. The round-6 "incomplete
   ISO-TP send" argument does not itself extend to a wait — the decisive
   argument is ADR-085's Alternative-#4 invariant: once teardown commits,
   this COP must always reach the terminal `Online`/`Finished` block, and by
   receive time the stop request is actually out (the ECU session is
   presumptively already torn down). Honoring a mid-receive cancel would
   force either `Cancelled` followed by `Finished` (contradictory statuses
   for one COP) or a bail-out stranding `comm_started`/`stop_comm_pending`
   `true` — the exact state ADR-085 refused to create. ADR-085's
   accepted-residual bullet (transient `GetStatus` `Cancelled` before the
   terminal `Finished`) already commits the client-visible "executing
   StopComm is not cancellable" model — but that acceptance was predicated
   on transience, which this receive phase's much longer window breaks
   (Codex review, PR #92: see Consequences). Rather than let the window
   widen, `wait_for_expected_response`'s per-pass check now actively drains
   any `cancelled_cops` entry on every poll pass regardless of
   `cancellable`, so `GetStatus(COP)` never reports `Cancelled` for a COP
   this non-cancellable phase is guaranteed to carry to `Finished` — only
   the ORIGINAL transmit step's shorter, still-transient window keeps
   ADR-085's acceptance unchanged. IS-CYCLIC's
   rejection removes the only truly deadline-free case; IS-MULTIPLE (`-2`)
   still needed its own bound, since its per-match deadline reset had no
   ceiling of its own — see Decision point 4. (The other codepaths are
   bounded by `CP_P2Max` restarts plus the fixed per-code RC ceilings,
   ADR-057.)
3. **A failed RC21/23 re-request does not end the COP with a bare
   `Finished`**: the error event is emitted (stale-gated), then control
   falls through to the terminal block — the same best-effort contract the
   initial transmit's `TxFailure::Event` already has.
4. **`NumReceiveCycles = -2` (IS-MULTIPLE) additionally carries an absolute
   `match_reset_ceiling_ms` bound** (added after this ADR's initial
   acceptance, Codex review of PR #92): `wait_for_expected_response`'s
   `CP_P2Max` window restarts on every accepted match with no ceiling of its
   own (point 2 above), and since this receive phase is non-cancellable, a
   chatty ECU — or a broad/empty `expected_response` pattern — could keep it
   running indefinitely with no escape short of a disconnect.
   `ExpectedResponseWait` gains `match_reset_ceiling_ms: Option<u32>`,
   computed once at receive-phase entry (`Some(max(16 x CP_P2Max, 2000ms))`
   for `-2` on the `CoptStopcomm` call site, `None` everywhere else) and
   anchored to a fixed `tokio::time::Instant` at that point — the same
   anchor-not-reset pattern ADR-057's RC21/23/78 completion ceilings already
   use, so a later match cannot push the ceiling out further. Each per-match
   deadline extension is then clamped to `deadline.min(ceiling)`; the RC
   ceilings are an independent mechanism and are untouched. `n > 0` does not
   get this ceiling (it is already bounded by its exact match count), and
   `CoptSendrecv`'s own IS-MULTIPLE receive phase does not get it either
   (`match_reset_ceiling_ms: None` at that call site) — it stays unbounded
   because it is cancellable via `CancelComPrimitive`, so a chatty ECU there
   always has a client-driven escape this phase lacks. Hitting the ceiling
   with at least one match already collected is indistinguishable from a
   normal IS-MULTIPLE window close: `matches_got > 0`, so no
   `PduErrEvtRxTimeout` fires.

Mechanically: `wait_for_expected_response` gains `cancellable: bool` on
`ExpectedResponseWait` (same name/semantics/threading as round 6's parameter
on `transmit_request`/`isotp_send`; `false` skips the per-pass
`cancelled_cops` consultation — the staleness half of that folded check
remains — and is forwarded to the re-request `transmit_request`), and
`ReceivePhaseOutcome` gains `ReRequestTxFailed` (error event already
emitted; the caller owns the terminal status — `handle_send_recv` emits
`PduCopstFinished` exactly as before, `handle_stop_comm` falls through to
its terminal block). `handle_send_recv` passes `cancellable: true`:
`CoptSendrecv`'s behavior is bit-for-bit unchanged.

Carriage: `TxItem::StopComm.tx` becomes `Option<StopCommTx>`, a bundle of
the existing `SendRecvTx` plus `expected_response`, `num_receive_cycles`,
`response_timeout_ms` (`p2_max_timeout_ms()`), and `RcHandlingConfig` — all
resolved at call time from the same bound Active snapshot (never Working;
`temp_param_update` stays a no-op for `CoptStopcomm`, ADR-044/ADR-067
unchanged). Bundling inside the `Some` payload makes "receive config without
a transmit" unrepresentable; empty `cop_data` stays byte-for-byte
pre-ADR-085 (no transmit, no receive, `LOCK_PHYSICAL_TX_QUEUE`-exempt;
`cop_ctrl_data` receive fields are documented as ignored there — nothing was
sent, so there is nothing a response could answer).

Guards: a pre-receive `still_on_this_channel` check (S3-equivalent,
mirroring `handle_send_recv`'s) is added between transmit success and the
wait. **Correction (ADR-086, generation-aware RX attribution round):** this
guard alone does not close the stale-attribution window it was originally
described as closing — it is a cheap, useful early bail for a reconnect that
has already completed by the time it runs, but `.await`s remain between it
and any individual poll pass's attribution decision inside
`wait_for_expected_response`'s shared engine, so a reconnect completing
later (during any subsequent pass, not just the first) could still slip a
new session's frame through a `target_cll`-only attribution check. The
actual closure of that window is a fix to the shared attribution layer
itself, not to this guard: `poll_rx_inner`'s `MatchProbe` arm now also
requires `connect_generation` equality before attributing a matched frame to
a `cop_handle`, closing the gap for `CoptSendrecv` and `CoptStopcomm` alike
(see ADR-086's dedicated round for this fix). No S4-equivalent is added: the
terminal block's channel-identity
check is already folded into the same lock acquisition as the
`comm_started`/`stop_comm_pending` clear (ADR-085 round 7 / ADR-086) and
serves that role. On a `Terminal` outcome (hard error or staleness — the
only ones reachable with `cancellable: false`), `handle_stop_comm` returns
early exactly like its existing `ChannelLost`/`HardError` arms; the flags
are owned by `handle_channel_hard_error` / the disconnect reset points on
those paths. `wait_for_p3_gap` and the post-send `TxGapState` bookkeeping
now receive the real `num_receive_cycles` (`no_response_required:
num_receive_cycles == 0`, ADR-060) instead of the hardcoded receive-less
values.

## Alternatives Considered

1. **Keep fire-and-forget (ADR-085 Alternatives #1)** — Superseded: its
   premise ("no fields to define a response") is factually wrong for the
   generic `PDU_COP_CTRL_DATA`/`ComPrimitiveCtrlData`, and discarding the
   fields loses the ECU's answer to the client's own final request.
2. **A bespoke StopComm receive loop instead of parameterizing
   `wait_for_expected_response`** — Rejected: same drift argument as ADR-085
   Alternative #5; the engine's matching/RC/timeout/staleness behavior must
   not fork.
3. **A genuinely cancellable receive phase** — Rejected: forces either
   contradictory `Cancelled`→`Finished` status sequences or stranding
   `comm_started`/`stop_comm_pending`; see Decision point 2.
4. **Honor a cancel by truncating the receive early but suppressing
   `PduCopstCancelled` and still emitting `Online`/`Finished`** — Rejected: a
   third bespoke knob inside the shared engine for marginal value, and a
   `CancelComPrimitive` that yields `PduCopstFinished` misreports what
   happened; the uniform non-cancellable window is simpler and matches the
   already-accepted residual.
5. **Allow IS-CYCLIC and rely on cancellation to end it** — Rejected:
   contradicts non-cancellability (point 2) and StopComm's must-terminate
   semantics; would wedge the FIFO poll task and the CLL pre-`Online`.
6. **Two independent top-level fields (`tx: Option<SendRecvTx>` plus
   optional receive config) on `TxItem::StopComm`** — Rejected: representable
   invalid state (receive config with `tx: None`); the bundle makes the
   invariant structural.
7. **Reject `-2` for `CoptStopcomm` like `-1`** (considered when adding
   `match_reset_ceiling_ms`) — Rejected: loses the functionally-addressed-stop
   use case (point 1 above) this ADR deliberately kept; the unbounded-drift
   problem has a narrower fix (point 4) that preserves it.
8. **Extend `match_reset_ceiling_ms` to `CoptSendrecv`'s IS-MULTIPLE receive
   phase too** — Rejected: that phase is cancellable, so the client always
   has an escape a chatty ECU there does not force it to lack; imposing an
   absolute ceiling on it anyway would be a broader behavior change than what
   the PR #92 review found, and would need its own justification separate
   from this ADR's non-cancellable-phase problem.

## Consequences

- A `CoptStopcomm` response matching `expected_response_array` is now
  reported via `ResultData` (`resultitem`) with `acceptance_id`/`cop_handle`
  attribution, identically to `CoptSendrecv`; `PduErrEvtRxTimeout` fires
  (stale-gated) when the required count does not arrive — after which the
  teardown still completes with `Online`/`Finished` (best-effort, ADR-085
  Alternative #4 unchanged).
- Contract change: `NumReceiveCycles`/`expected_response_array` on a
  non-empty-`cop_data` `CoptStopcomm` were previously silently ignored; they
  now take effect, and `NumReceiveCycles < -2` or `== -1` is now a
  synchronous `INVALID_ARGUMENT` (previously accepted and ignored). Empty
  `cop_data` behavior is unchanged, including ignoring these fields.
- The `comm_started == true` / `stop_comm_pending == true` window widens
  again (ADR-085 amendment 1 call-out extended): it now spans the receive
  phase — up to `CP_P2Max` restarts for `n > 0`, up to
  `max(16 x CP_P2Max, 2000ms)` for IS-MULTIPLE (`match_reset_ceiling_ms`,
  Decision point 4), plus the RC21/23/78 completion ceilings
  (client-configured; default 5 s each, ADR-057), each applied once (the
  RC ceilings and `match_reset_ceiling_ms` are independent mechanisms and do
  not stack against each other on the same match). No new race: the second-
  StopComm guard is `stop_comm_pending` (unaffected), and `CoptStartcomm`
  rejection during the window is correct — teardown genuinely is not
  finished until the response phase ends. `GetStatus` reporting
  `CommStarted` throughout is likewise truthful.
- `ExpectedResponseWait` gains `match_reset_ceiling_ms: Option<u32>`
  (Decision point 4): `None` for `handle_send_recv`'s call site (no
  behavior change to `CoptSendrecv`) and for `CoptStopcomm`'s own `0`/`n > 0`
  paths; `Some(max(16 x CP_P2Max, 2000ms))` only for `CoptStopcomm`'s `-2`
  path.
- The non-cancellable poll-task occupancy window widens by the same bound —
  sibling CLLs on a shared channel wait longer behind a responding
  StopComm; mode-1 idle tester-present siblings are exempt (the per-pass
  `dispatch_due_idle_tester_present` inside the wait serves them).
  Accepted residual, same class as round 6's window.
- `wait_for_expected_response`/`ReceivePhaseOutcome`/`TxItem::StopComm`
  signatures change; `handle_send_recv`'s path is behavior-identical
  (`cancellable: true`, and `ReRequestTxFailed` re-emits the same
  `Finished` at the call site).
- **Codex-review fix (PR #92):** the per-pass cancellation check inside
  `wait_for_expected_response` now drains a `cancelled_cops` entry on
  every pass regardless of `cancellable`, not only when `cancellable` is
  `true`. `rpc_get_status`'s `CopHandle` branch checks `cancelled_cops`
  before `executing_cop` (Priority: Cancelled > Executing > Waiting), so
  the original `cancellable: false` design — leaving the entry for
  `dispatch_tx_item`'s later post-completion cleanup, mirroring round 6's
  `transmit_request`/`isotp_send` handling — made `GetStatus(COP)` report
  `PduCopstCancelled` for the entire, now-potentially-multi-second receive
  phase after an ignored cancel, not merely "transiently" as ADR-085's
  accepted residual assumed for the much shorter transmit step. Draining
  the entry (while still not honoring it — `cancellable` still gates
  whether it terminates the wait) closes this without changing the
  receive phase's actual non-cancellable behavior; `HashSet::remove` is a
  no-op on an absent key, so redundant draining on later passes is free.
  `CoptSendrecv`'s `cancellable: true` path is unaffected (it already
  drained and honored the entry every pass). The original transmit
  step's own `cancellable: false` window (`transmit_request`/`isotp_send`)
  is untouched and keeps ADR-085's existing acceptance, since it remains
  genuinely short.
- **Codex-review fix, round 2 (PR #92):** the drain above only ran at each
  loop iteration's per-pass check — a single, un-chunked
  `tokio::time::sleep(request_time_ms)` inside the RC21/23 pending-response
  handling (before that iteration's retransmit) meant a cancel arriving
  during that sleep still went un-drained until the sleep (plus the
  retransmit after it) finished, for up to the client-configured
  `CP_RC21RequestTime`/`CP_RC23RequestTime` (potentially hundreds of ms or
  more) — the same class of gap round 1's fix closed for the rest of the
  loop, just narrower in scope. Fixed by chunking that sleep into
  `POLL_INTERVAL_MS`-sized steps (mirroring `wait_for_p3_gap`'s existing
  chunked-wait shape) and draining `cancelled_cops` on every chunk boundary
  when `!cancellable`, without changing the sleep's total duration or
  `CoptSendrecv`'s (`cancellable: true`) timing at all. The remaining
  window — up to one `POLL_INTERVAL_MS` (10 ms) after a cancel, plus
  whatever real gRPC/TCP round-trip latency separates the `CancelComPrimitive`
  and a following `GetStatus` call in practice — is the same order of
  magnitude as the original transmit step's own accepted-as-transient
  residual, and is left as-is rather than chased further.
- `docs/rpc-api-guide.md`'s `COPT_STOPCOMM` contract,
  `docs/j2534-0404-architecture.md`'s `TxItem` listing, and
  `j2534-0404-service/docs/adapter-design.md`'s COP model are updated in
  the same commit; ADR-085 receives the `*Update (ADR-087)*` note under
  Alternatives #1.
