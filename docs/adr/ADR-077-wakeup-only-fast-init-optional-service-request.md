# ADR-077: Wakeup-Only Fast-Init — the Fast-Init Service Request is Optional

**Date:** 2026-07-10
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service.rs` (new `FastInit`, `TxItem::StartComm::fast_init`),
`j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`'s
`CoptStartcomm` arm), `j2534-0404-service/src/service/events.rs`
(`StartCommParams`, `handle_start_comm`, `run_protocol_init`,
`select_init_sequence`'s doc comment), `j2534-0404-mock/src/lib.rs`
(`IOCTL_FAST_INIT` handler, `ChannelState::fast_init_input`),
`j2534-0404-service/tests/grpc_mock/startcomm_comparam.rs`

## Context

ISO 22900-2 defines `PDU_COPT_STARTCOMM` fast initialization as a wakeup
pattern that may be followed by a start-communication service request:
`init_data` (`cop_data`) carrying that request is explicitly OPTIONAL in the
spec, not mandatory. ADR-074 and ADR-075 both assumed the opposite for the
empty-`cop_data` case: ADR-074's `select_init_sequence` treats an absent
`CP_InitializationSettings` (the legacy heuristic) plus empty `init_data` as
still selecting `InitSequence::Fast` in principle, but every call site that
actually dispatches an init step gated on `!cop_data.is_empty()` first — so
in practice, before this ADR, an empty `cop_data` always meant "skip the init
call entirely," regardless of `CP_InitializationSettings`. There was no way
for a client to request the D-PDU-spec-legal "wakeup pattern only, no
request" behavior at all.

## Decision

Wakeup-only fast-init now fires when, and only when, the bound ComParam
snapshot has `CP_InitializationSettings` **explicitly** set to `2` (Fast) —
`binding.resolved().unum32.get(&PARAM_INIT_SETTINGS) == Some(2)` — **and**
`cop_data` is empty, on a K-line link (ISO9141/ISO14230). The check is on
this explicit bound value, not on `select_init_sequence`'s result: the
legacy heuristic (param unset) also resolves to `InitSequence::Fast` for
empty `init_data` on ISO14230, and that path's empty-`cop_data` behavior
stays "skip init entirely," unchanged, for backward compatibility. Explicit
`CP_InitializationSettings=3` still skips init regardless of `cop_data`.
Explicit `CP_InitializationSettings=1` is unaffected — it remains the
ADR-076 spec 5-baud contract — and wakeup-only fast-init and 5-baud init stay
call-time-exclusive: an explicit `2` can never produce a `five_baud` value.

A new `FastInit` enum (`j2534-0404-service/src/service.rs`, alongside
`FiveBaudInit`) replaces `TxItem::StartComm`'s former `fast_init_frame:
Vec<u8>` field with `fast_init: Option<FastInit>`:

- `FastInit::WakeupOnly` — no request; the wakeup pattern alone is sent.
- `FastInit::WithRequest(Vec<u8>)` — the pre-built KWP wakeup frame
  (ADR-075's existing call-time construction, unchanged for this case).

`None` at the `TxItem::StartComm::fast_init` field means no fast-init at
all (five-baud, skip, or non-K-line) — the same role the old empty `Vec`
played, but the poll task no longer infers "no fast-init" from frame
emptiness; it inspects the variant directly. This closes off the
representational trap the old `Vec<u8>` encoding would have created for
wakeup-only (an empty frame could otherwise have meant either "no fast-init"
or "fast-init with no request," which the poll task cannot tell apart from
the byte length alone).

At `StartComPrimitive` call time (`rpc_start_com_primitive`), the existing
non-empty-`cop_data` fast-init path is unchanged (`build_tx_message` plus the
ADR-049 SAE J2534-1 TX size validation, now wrapped in
`FastInit::WithRequest`). The new wakeup-only path builds no frame and runs
no size validation at all — there is no payload to validate.

In the poll task (`handle_start_comm`/`run_protocol_init`):

- The init-step guard becomes `five_baud.is_some() || fast_init.is_some()`.
- `WakeupOnly` calls `api.fast_init(channel_id, None)` — no
  `PassThruMessage` is constructed, and `init_tx_flags` is irrelevant to this
  branch (there is no message to carry flags).
- `WithRequest(frame)` is the unchanged ADR-075 behavior.
- **Response delivery is skipped entirely for `WakeupOnly`**: no
  `ReceivedFrame` is pushed to the CLL's receive buffer, and no `ResultData`
  `SubscribeEvent` notification is sent — per the D-PDU API spec, fast-init
  "behaves like SendRecv only if a request message is contained," so with no
  request there is no response to deliver. This is gated on the `FastInit`
  variant (via the existing `deliver_keybytes` boolean, now derived from the
  variant instead of always being `true` for fast-init), not on whether the
  adapter's returned bytes happen to be empty — an adapter that returns
  stray data on a wakeup-only call must not have it surface as a response.
  `CP_Baudrate` write-back stays 5-baud-only, unaffected.
  `PduCllstCommStarted`/`PduCopstFinished`, temp-param apply/revert, and
  tester-present start are all unaffected by which `FastInit` variant ran.

The mock (`j2534-0404-mock`) already accepted a NULL `p_input` to
`IOCTL_FAST_INIT` (tests predate this ADR), but always wrote its canned
`MOCK_FAST_INIT_RESPONSE` into a non-null output regardless, and never reset
`ChannelState::fast_init_input` back to `None` on a subsequent NULL-input
call. Both are corrected: a NULL `p_input` now writes `DataLength = 0` to a
non-null output (no request sent, so nothing for the ECU to answer), the
init counter still increments, and `fast_init_input` is explicitly reset to
`None`, so tests can distinguish "wakeup-only fast-init ran" (`fast_init_count()`
incremented, `fast_init_input()` returns `None`) from "no fast-init at all"
(`fast_init_count()` unchanged) using the two mock accessors together.

## Consequences

- **Amends ADR-074 and ADR-075's empty-`cop_data`-means-skip-init
  assumption**, annotated in place in both — the explicit
  `CP_InitializationSettings=2` case is now a narrow exception; the
  legacy/absent-param path's empty-`cop_data` behavior is explicitly
  unchanged (a regression here would be a silent backward-compatibility
  break, not a wakeup-only case).
- **`select_init_sequence` itself is unchanged.** Its `Fast` result for empty
  `init_data` under the legacy heuristic was already latent (ADR-074 never
  claimed it meant "run fast-init on empty data" — every dispatch site
  independently gated on `!cop_data.is_empty()`); this ADR makes the
  distinction explicit in its doc comment rather than changing its logic,
  since changing it would risk altering the legacy path.
- **`TxItem::StartComm::fast_init_frame: Vec<u8>` is now `fast_init:
  Option<FastInit>`** — a breaking change to this crate-internal type only
  (not the gRPC surface), touching every call/construction site in
  `rpc_primitive.rs` and `events.rs`.
- **ISO9141's explicit-fast-init behavior is unaffected**: per ADR-074,
  `FAST_INIT` is valid on both K-line protocols, so an ISO9141 link with
  `CP_InitializationSettings=2` and empty `cop_data` gets wakeup-only
  fast-init the same as ISO14230 — no protocol-specific special-casing, same
  as the existing `WithRequest` path (an adapter that rejects `FAST_INIT` on
  a given protocol surfaces its own error through the normal init-failure
  path, unchanged).
