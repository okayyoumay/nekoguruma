# ADR-076: D-PDU 5-Baud Init Contract for Explicit CP_InitializationSettings=1

**Date:** 2026-07-10
**Status:** Accepted (the legacy-FiveBaud-path unconditional key-byte delivery -- the "Behavioral asymmetry, by design" consequence -- narrowed by ADR-183: it no longer covers PROTOCOL_UART_ECHO_BYTE_PS, whose sole init path gains NumReceiveCycles gating)
**Affects:** `j2534-0404-service/src/service.rs` (`TxItem::StartComm`, new
`FiveBaudInit`), `j2534-0404-service/src/service/rpc_primitive.rs`
(`rpc_start_com_primitive`'s `CoptStartcomm` arm),
`j2534-0404-service/src/service/events.rs` (`handle_start_comm`,
`run_protocol_init`), `j2534-0404-service/src/service/rpc_link.rs`
(`rpc_set_com_param`), `j2534-0404-mock/src/lib.rs` (`IOCTL_FIVE_BAUD_INIT`
handler), `j2534-0404-service/tests/grpc_mock/startcomm_comparam.rs`

## Context

ISO 22900-2 defines a specific client contract for a 5-baud initialization
`PDU_COPT_STARTCOMM`:

- the tester sends the **ECU address from ComParams** on the bus
  (`CP_5BaudAddressPhys` / `CP_5BaudAddressFunc`, with `CP_5BaudMode`
  selecting the handshake variant),
- **no optional message is allowed** in the start communication request,
- the ECU key bytes are returned as a result message
  (`PDU[0]=KB1, PDU[1]=KB2`) **only when `NumReceiveCycles` is set to 1**,
- afterwards `PDUGetComParam(CP_Baudrate)` returns the **baud rate the
  interface calculated** during the init sequence, and
- keep-alive (tester-present) begins if enabled.

After ADR-074 gave `CP_InitializationSettings` control over *which* init
sequence runs, the FiveBaud execution itself still predated that contract:
the address was taken from `cop_data[0]` (client-supplied), a non-empty
`cop_data` was in fact *required* (the poll task's `!cop_data.is_empty()`
guard treated an empty `cop_data` as "skip init"), the key bytes were always
delivered regardless of `NumReceiveCycles` (ignored for `CoptStartcomm`),
and the calculated baud rate was never read back — `CP_5BaudAddressFunc`
(`0x807F`) and `CP_5BaudAddressPhys` (`0x8080`) were seeded by the K-line
presets but never read anywhere.

The spec contract and the legacy behavior cannot coexist on one code path:
the ADR-074 legacy heuristic selects FiveBaud *because* `cop_data` is a
single byte, so it cannot simultaneously reject a non-empty `cop_data` as
the spec demands.

## Decision

**The spec contract applies exactly when `CP_InitializationSettings == 1`
is explicitly present in the COP's bound ComParam snapshot
(`binding.resolved()`, ADR-067) and the link's `hw_protocol_id` is K-line
(ISO9141/ISO14230).** The absent-param legacy-heuristic FiveBaud path keeps
its pre-existing behavior byte-for-byte, per ADR-074's stance that the
absent-param fallback is "a deliberate compatibility shim, not the long-term
contract". Explicit `=1` on a non-K-line link is unchanged (no init sequence
ever runs there, so none of the new validation applies).

Under the spec contract, resolved synchronously at `StartComPrimitive` call
time (ADR-067 — no ComParam resolution at poll-task execution time):

1. **No optional message:** a non-empty `cop_data` is rejected with
   `Status::invalid_argument`. The `cop_data` field *is* the D-PDU "optional
   message" for this COP; the address comes from ComParams, so any payload
   is by definition redundant.
2. **Address from ComParams:** `CP_RequestAddrMode == 2` (functional,
   ADR-054) selects `CP_5BaudAddressFunc` (default `0x33` when absent),
   otherwise `CP_5BaudAddressPhys` (default `0x01` when absent) — the
   defaults match the K-line preset seeds. A bound value `> 0xFF` is
   rejected (`invalid_argument`); the address is a single byte on the bus.
   `SetComParam` also rejects `> 0xFF` for both params up front, mirroring
   ADR-074's `CP_InitializationSettings` range check.
3. **`NumReceiveCycles` gates key-byte delivery:**
   `cop_ctrl_data.num_receive_cycles == 1` delivers the raw `[KB1, KB2]`
   result (rx_buf + `ResultData` event, no `extra_info`, as before);
   `0`/absent runs the init but suppresses the delivery; any other value
   (including negatives — the field is signed) is rejected synchronously.
4. **Empty `cop_data` no longer means "skip init" on this path:** the init
   runs. "Skip" is expressed by `CP_InitializationSettings = 3`
   (`InitSequence::None`, ADR-074).

The resolved outcome is carried in a new
`TxItem::StartComm::five_baud: Option<FiveBaudInit { address, deliver_keybytes }>`
field; the legacy path populates it with `cop_data[0]` /
`deliver_keybytes = true` (only when `cop_data` is non-empty, preserving its
skip semantics). The poll task's init-step guard becomes
`five_baud.is_some() || !fast_init_frame.is_empty()`, and
`run_protocol_init` dispatches on the carried data — the execution-time
`select_init_sequence` call is gone, making call-time/execution-time
agreement structural instead of relying on the selector's purity (ADR-075).

> **Amended by ADR-077:** `fast_init_frame: Vec<u8>` becomes `fast_init:
> Option<FastInit>` (`FastInit::WakeupOnly` / `FastInit::WithRequest(Vec<u8>)`),
> and the guard becomes `five_baud.is_some() || fast_init.is_some()` — keyed
> on the `Option`, not on frame emptiness, because a wakeup-only fast-init
> (explicit `CP_InitializationSettings == 2` with empty `cop_data` on a
> K-line link) has no frame to check for emptiness at all.

**`CP_Baudrate` write-back:** after a *successful* five-baud init (spec and
legacy paths alike — the negotiated baud is a hardware fact independent of
how the address was sourced; not fast-init, which negotiates no baud), the
service reads `GET_CONFIG DATA_RATE` from the adapter and writes it into
**both** the Working and Active sets under `CP_Baudrate`
(`j2534_0404::DATA_RATE`). Both sets is required, not cosmetic: writing only
Active would leave `Working != Active` on a `PDU_PC_BUSTYPE`-class param and
spuriously trip ADR-067's `PDU_ERR_TEMPPARAM_NOT_ALLOWED` pre-check on the
next `temp_param_update` COP. This is an execution-time ComParam **write**
of a hardware-measured fact, not a resolution, so it does not violate
ADR-067's call-time-binding invariant. No ordering hazard exists against the
temp-init revert: `apply_params_to_hardware` filters `DATA_RATE`
unconditionally (ADR-011), so `revert_hardware_to_live_active` never pushes
a baud rate and cannot clobber the freshly negotiated link. A `GET_CONFIG`
failure after a successful init logs a warning and continues — the link is
up; the readback is best-effort.

**`CP_ExtendedTiming` stays out of scope.** J2534-1 v04.04 exposes no
extended-timing config parameter; synthesizing a P2/P3 `SET_CONFIG` mapping
is exactly the kind of unsupported-feature invention ADR-017 rules out. The
ComParam remains settable; a client whose key bytes indicate extended timing
overrides the concrete timing ComParams (e.g. `CP_P2Max`) itself, which map
to real `SET_CONFIG` parameters. Documented in `rpc-api-guide.md`.

> **Revised 2026-07-10 (ADR-077 review):** the categorical "unsupported-
> feature invention" ruling above is too broad. ISO 22900-2 *does* define
> extended timing for ISO 14230-2 (key-byte-gated timing set), so
> adapter-layer handling of it is legitimate scope, not invention — J2534-1's
> silence on a dedicated extended-timing IOCTL doesn't preclude mapping it
> onto the existing per-parameter `SET_CONFIG` timing IDs (e.g. `CP_P2Max`)
> once the key bytes indicate it applies. Implementation remains deferred
> pending additional information (key-byte inspection to detect
> extended-timing support, and the exact value mapping), tracked as a P2 item
> in `j2534-0404-service/docs/implementation-notes.md`'s backlog rather than
> ruled out here.

## Consequences

- **Intentional breaking change for explicit-`=1` clients** that passed the
  address in `cop_data`: they now get a synchronous `invalid_argument` and
  must set `CP_5BaudAddressPhys`/`Func` (+ `CP_RequestAddrMode`) and send an
  empty `cop_data` with `NumReceiveCycles = 1` — the same migration stance
  ADR-075 took for fast-init clients. Legacy (absent-param) clients are
  untouched.
- **Behavioral asymmetry, by design:** `NumReceiveCycles` gating and the
  empty-`cop_data`-runs-init semantics apply only to the spec path; the
  legacy FiveBaud path and fast-init (ADR-075) keep delivering their
  responses unconditionally. Extending the gate to those paths would change
  contracts the spec text does not cover.
- **Refines ADR-074's `InitSequence::FiveBaud` bullet** ("calls
  `five_baud_init` with `init_data[0]`") — that description now applies only
  to the absent-param legacy path — and **ADR-075's "FiveBaud … still uses
  `cop_data[0]` directly, unaffected" statements**, same partial-refinement
  precedent ADR-075 itself set on ADR-050 (annotated in place, no `Status`
  change).
- `TxItem::StartComm` keeps its `cop_data` field solely for the
  "init data provided for a protocol that doesn't require init" diagnostic;
  the init step no longer reads it.
- The measured-baud write-back can overwrite a staged Working `CP_Baudrate`
  edit that the client had not yet promoted; the negotiated rate is ground
  truth for the now-live link (baud is fixed post-connect, ADR-011), so this
  is accepted.
- **Mock (`j2534-0404-mock`):** `IOCTL_FIVE_BAUD_INIT` now records its input
  address byte (`mock_get_five_baud_init_input`, mirroring ADR-075's
  `mock_get_fast_init_input`) and seeds `DATA_RATE` to
  `MOCK_FIVE_BAUD_NEGOTIATED_BAUD` (10 400 — the classic ISO 9141-2 rate),
  simulating the interface's baud calculation so tests can assert the
  `GetComParam(CP_Baudrate)` readback end-to-end.
