# ADR-075: Fast-Init Header Construction at Call Time and Response Header/Footer Parsing

**Date:** 2026-07-10
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`rpc_start_com_primitive`'s
`CoptStartcomm` arm), `j2534-0404-service/src/service.rs` (`TxItem::StartComm`),
`j2534-0404-service/src/service/events.rs` (`StartCommParams`, `handle_start_comm`,
`run_protocol_init`), `j2534-0404-mock/src/lib.rs` (`IOCTL_FAST_INIT` handler),
`j2534-0404-service/tests/grpc_mock/startcomm_comparam.rs`

## Context

ADR-050 gave `CoptSendrecv`/`CoptStartcomm`'s periodic tester-present message
symmetric, ComParam-derived header construction on TX, and ADR-051 gave every
received frame a matching header/footer split into `ResultData.extra_info` on
RX — except one place: `CoptStartcomm`'s `init_data` (`cop_data`). ADR-050
explicitly scoped `init_data` out ("it remains raw, client-supplied bytes"),
and `handle_start_comm`'s fast-init response delivery (`run_protocol_init`'s
`InitSequence::Fast` branch) pushed the adapter's raw response bytes straight
into `data_bytes` with no `extra_info` at all — the one COP-data path in this
service without the header/footer symmetry the rest of `CoptSendrecv`/
`CoptStartcomm` already has.

This forced fast-init clients to hand-build the KWP2000 wakeup header
(format/target/source/length bytes) themselves — duplicating ComParam-derived
information the service already has, the exact problem ADR-050 solved for
`CoptSendrecv` — and to parse the adapter's raw StartCommunication response
frame themselves, with no way to distinguish header from payload the way
every other received frame in this service already lets them.

FiveBaud init (`CP_InitializationSettings=1`) is unaffected: its wakeup
payload is a single target-address byte with no header concept, and its
adapter response (`five_baud_init`'s fixed `[KB1, KB2]` keyword bytes) has no
header/footer structure either — `IOCTL_FIVE_BAUD_INIT` already returns
exactly the client-meaningful bytes. **Superseded in part by ADR-076:** the
address byte's *source* stops being uniformly "the client's `init_data`" —
under the spec-mandated 5-baud contract (explicit `CP_InitializationSettings
== 1` on a K-line link), the address instead comes from ComParams
(`CP_5BaudAddressFunc`/`CP_5BaudAddressPhys`) and a non-empty `init_data` is
rejected. The no-header/no-footer, raw-`[KB1, KB2]` observation itself still
holds on both paths.

## Decision

For K-line fast-init (`InitSequence::Fast`, ISO9141/ISO14230 only —
`protocol_requires_init` never selects an init sequence for any other
protocol), `cop_data` is now **payload-only**, matching `CoptSendrecv`'s
`cop_data` contract:

- **TX (call time):** `rpc_start_com_primitive`'s `CoptStartcomm` arm builds
  the fast-init wakeup frame via `tx_header::build_tx_message`, synchronously,
  right alongside `init_tx_flags`'s existing call-time resolution
  (`resolve_init_tx_flags`, ADR-067) — from `binding.resolved()` (Working
  when `temp_param_update` is set, else Active) and the same
  `bound_active_table` UniqueRespIdTable snapshot. This is deliberately
  call-time, not execution-time, consistent with ADR-067/068's "no
  resolution left at poll-task execution time" model: nothing about fast-init
  header construction should behave any differently from `init_tx_flags`,
  which already runs at this exact point. The result (`fast_init_frame:
  Vec<u8>`) is carried through `TxItem::StartComm` and `StartCommParams` to
  `handle_start_comm`, which passes it to `run_protocol_init` in place of
  `cop_data` for the `Fast` branch only — `FiveBaud` still uses
  `cop_data[0]` directly, unaffected. **Superseded in part by ADR-076:** that
  is only the pre-existing absent-`CP_InitializationSettings` legacy
  heuristic path; the spec-mandated `CP_InitializationSettings == 1` path
  resolves its address from `CP_5BaudAddressFunc`/`CP_5BaudAddressPhys`
  instead and requires an empty `cop_data`. A `build_tx_message` error at call time
  maps to the same synchronous `Status::invalid_argument` the `CoptSendrecv`
  call site (`resolve_send_recv_tx`) and `resolve_tester_present` already
  use.
  - Non-K-line links, or an empty `cop_data` ("skip init," unchanged
    semantics), both get an empty `fast_init_frame` — the poll task's
    `!cop_data.is_empty()` skip-init guard stays keyed on the raw `cop_data`,
    not the constructed frame, so this is purely additive.
    > **Amended by ADR-077:** "an empty `cop_data`... skip init" is no longer
    > universally true. `fast_init_frame: Vec<u8>` becomes `fast_init:
    > Option<FastInit>`, and an **explicit** `CP_InitializationSettings == 2`
    > with empty `cop_data` on a K-line link now populates
    > `Some(FastInit::WakeupOnly)` — a wakeup-only fast-init with no request
    > and no frame construction — instead of `None`. The absent-param legacy
    > heuristic's empty-`cop_data` behavior is unchanged (still `None`, still
    > "skip init entirely"). The poll task's guard becomes
    > `five_baud.is_some() || fast_init.is_some()`, keyed on the `Option`, not
    > on frame emptiness.
- **RX (response delivery):** the `Fast` branch's response bytes are now run
  through the same `header_footer_len`/split logic `poll_rx_inner` already
  applies to every ordinary received frame (ADR-051) — KWP is the only
  protocol `run_protocol_init` ever executes against, so `can_addressing_by_id`
  (only relevant to ISO15765) is passed as an empty slice. The result is
  delivered exactly like any other KWP frame: payload in
  `ReceivedFrame.data`/`ResultData.data_bytes`, header/footer (when
  non-empty) in `ReceivedFrame.header_bytes`/`footer_bytes` and
  `ResultData.extra_info`. `FiveBaud` keybytes are delivered raw, as before —
  no split, no `extra_info`.

## Consequences

- **Intentional breaking change for fast-init clients that embedded the KWP
  header in `init_data` or parsed the raw response frame themselves** —
  mirrors ADR-050's own stance on `CoptSendrecv`/tester-present ("a
  deployment that previously embedded a CAN ID/header... must migrate"). A
  fast-init client must now call `SetUniqueRespIdTable`/`SetComParam` for its
  addressing (same as any other K-line request) and send payload-only
  `init_data`, and must read `ResultData.extra_info` for the response's
  header/footer instead of assuming `data_bytes` is the raw frame.
- **Partially supersedes ADR-050's `init_data` scope bullet** ("it remains
  raw, client-supplied bytes") — annotated in place, without changing
  ADR-050's overall `Status` (a partial supersede, the same precedent
  ADR-054's annotation on ADR-050's functional-addressing bullet set).
- **Refines ADR-074's `InitSequence::Fast` bullet**, which described
  `init_data` as the literal wakeup frame passed to `fast_init` — that
  description now applies to `fast_init_frame` (the pre-built frame), not
  `cop_data`/`init_data` itself; `run_protocol_init`'s dispatch logic is
  otherwise unchanged by this ADR.
- **Mock (`j2534-0404-mock`):** `IOCTL_FAST_INIT` now records its input frame
  (`PASSTHRU_MSG.Data[..DataSize]`) per channel, exposed via
  `mock_get_fast_init_input`/`__mock_get_fast_init_input`, mirroring the
  existing `written_msgs` recorded-write pattern — lets tests assert the
  constructed header's exact addressing. Its canned response also changes
  from the 5-baud-init keyword bytes (`MOCK_INIT_RESPONSE`, unchanged, still
  used by `IOCTL_FIVE_BAUD_INIT`) to a distinct, realistic ISO14230
  StartCommunication positive response (`MOCK_FAST_INIT_RESPONSE =
  [0x83, 0xF1, 0x10, 0xC1, 0xE9, 0x8F]`: format `0x83` = addressed + embedded
  `LEN=3`, target `0xF1`, source `0x10`, payload `C1 E9 8F`, no checksum byte
  — vendor-managed, same assumption as every other header construction in
  this service), enabling a genuine round-trip header/payload split
  assertion instead of an arbitrary 3-byte blob.
- **The constructed init frame gets the same SAE J2534-1 size-range
  rejection as `CoptSendrecv`'s constructed message (ADR-049)**, applied
  synchronously at `StartComPrimitive` call time. Without it, an oversized
  payload (e.g. 256 bytes on ISO14230, whose constructed 260-byte frame
  exceeds the 259-byte TX ceiling) would truncate the KWP length byte into
  a malformed frame and surface only as an async init failure. K-line never
  uses extended addressing, so the Normal range applies. Because ISO9141's
  SAE frame ceiling (4128) is far wider than the single KWP length byte can
  encode, `build_tx_message`'s KWP arm additionally rejects any payload over
  255 bytes before building the header — this guard sits at the shared
  construction site, so it also closes the same pre-existing silent
  length-byte wrap on the `CoptSendrecv`/tester-present ISO9141 paths, not
  just fast-init. Construction and
  rejection are **gated on the bound snapshot actually selecting fast-init**:
  `select_init_sequence` is a pure function of `binding.resolved()` +
  `cop_data`, so evaluating it at call time always agrees with the poll
  task's own execution-time selection, and a five-baud (`init_data[0]`
  only) or skip-init COP is never rejected over a frame it would not send.
- **Functional addressing now reaches the init frame.** Because the header
  goes through the same `build_tx_message`, a link with
  `CP_RequestAddrMode = 2` (ADR-054) gets a functionally addressed
  StartCommunication header (`CP_FuncReqFormatPriorityType`/
  `CP_FuncReqTargetAddr`, KWP defaults `0xC0`/`0x33`) instead of a physical
  one — before this ADR the client's raw `init_data` was sent verbatim
  regardless of that ComParam. KWP2000 fast-init StartCommunication is
  conventionally physical; a client wanting the conventional wakeup keeps
  the default physical addressing. Documented in `rpc-api-guide.md`'s
  fast-init contract section.
- No change to `FiveBaud`'s wire contract, `select_init_sequence`'s
  ComParam-driven sequence selection (ADR-074), or `InitSequence::None`'s
  skip-init behavior. **Superseded in part by ADR-076:** the explicit
  `CP_InitializationSettings=1` FiveBaud contract has since changed
  (ComParam-sourced address, empty `cop_data` required, `NumReceiveCycles`
  gating, `CP_Baudrate` write-back); the absent-param legacy FiveBaud path,
  the sequence selection, and `InitSequence::None` remain unchanged.
