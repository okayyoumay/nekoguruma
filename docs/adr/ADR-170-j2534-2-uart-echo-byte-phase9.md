# ADR-170: SAE J2534-2 UART Echo Byte Protocol (Phase 9)

**Date:** 2026-08-11
**Status:** Accepted (Consequences' accepted-residual bullet on the ten `UEB_T*` timing parameters' missing client-facing access path closed by [ADR-216](ADR-216-native-only-comparam-exposure-analog-uart-echo-byte.md))
**Affects:** `j2534-0404-service/src/service/protocol.rs`, `resources.rs`,
             `names.rs`, `comparam_support.rs`, `rpc_misc.rs`,
             `j2534-0404-mock`

## Context

`docs/j2534-2-support-plan.md`'s Phase 9 covers SAE J2534-2 clause 12, UART
Echo Byte Protocol: Honda ABS/VSA (SAE J2809) or KWP1281 (SAE J2818), a
K-line UART physical layer distinguished by a per-byte echo/complement
handshake between tester and ECU. New `ProtocolID`s `UART_ECHO_BYTE_PS`
(no unqualified base id — clause 12.3.3.1.4's table defines only `_PS`/
`_CHx`) and `UART_ECHO_BYTE_CHx`. Phase 0 (ADR-152) already added the raw
constants to `j2534-0404-sys/src/bindings/j2534_v0404.h`:
`PROTOCOL_UART_ECHO_BYTE_PS` (`0x0000800A`), ten `CONFIG_UEB_T{0-9}_{MIN,MAX}`
IDs, and three `DEVICE_INFO_UART_ECHO_BYTE_*` discovery bits — no header
edit or bindgen regeneration is needed for this phase.

**This is not a CAN-family variant, unlike every prior `_PS`-only J2534-2
protocol this codebase has implemented.** CAN FD (ADR-158/159), Single Wire
CAN (ADR-164), and Fault-Tolerant CAN (ADR-168) are each framed by their own
spec clause as the same as `CAN_PS`/`ISO15765_PS` apart from the
differences it lists explicitly — so each reuses the existing `ChannelProtocol::CAN`/`ISO15765`
identity via a resource row's `hw_protocol_override`, resolved back to its
base id by `resources::base_protocol_id`. Clause 12 carries no such framing:
UART Echo Byte is a wholly distinct message format (the echo-byte scheme,
`Data[0]` = length / `Data[1]` = message counter / `Data[2]` = message title
/ ETX-terminated, clause 12.4.2 Table 38) with no relationship to CAN,
ISO15765, or even ISO9141's own D-PDU semantics beyond sharing a K-line
physical layer type. Per ADR-023's original rule ("a phase introducing a
genuinely new D-PDU protocol needs a variant"), this is that case — the same
category ISO9141/J1850PWM/J1850VPW/SCI already occupy as their own standalone
`ChannelProtocol` variants, not the FD/SW/FT-CAN override pattern.

**D-PDU resource mapping has no ISO 22900-2 precedent at all.** Unlike
SWCAN (Annex G explicitly lists Single-Wire CAN as a CAN bus-type variant)
and FT-CAN (ISO 22900-2 models `ISO_11898_3_DWFTCAN` as its own bus type),
a direct search of both available ISO 22900-2 editions
(`iso-22900-2/ISO_22900-2_2009(E)-Character_PDF_document.md`,
`iso22900-2-2022/ISO_22900-2_2022(en).md`) for "Honda", "VSA", "KWP1281",
"J2809", and "J2818" returns zero matches — this protocol is Honda/VW/Audi
proprietary and ISO 22900-2 defines no resource for it. This does not block
adding a resource-table row, however: the `0x0200` resource-ID namespace
itself is this project's own invention (ADR-069, "the opaque `0x0200`
resource-ID namespace this ADR introduces"), not something ISO 22900-2
defines — ADR-069's rows simply alias ISO 22900-2's own standard short
names where one exists. A row with no ISO short name to alias just uses a
project-chosen descriptive `protocol_name` instead; the numeric-ID
allocation mechanism (next sequential id in the namespace) is unaffected.

**A latent ComParam-allowlist gap would otherwise misclassify this
protocol.** `comparam_support.rs::is_param_allowed`'s fallback for a
protocol matching none of the existing `is_*_family()`/exact-identity
checks is "Unknown protocol — allow to avoid rejecting custom/future
protocols." Without an explicit classification, a bare new
`ChannelProtocol::UartEchoByte` variant would fall through to that
fallback and make *every* ComParam settable/gettable on it — CAN, KWP,
tester-present, all of it — directly contradicting clause 12.3.4.1's own
closed parameter list, which names only `SAE_J1962_PINS` and `DATA_RATE`
as supported and excludes every other native J2534-1 config parameter.
This is the
phase's one correctness-sensitive design point, resolved directly
below, the same way ADR-164's own reconnaissance surfaced and resolved a
SWCAN-specific structural question without an escalation.

**The ten new `SET_CONFIG`/`GET_CONFIG` timing parameters
(`UEB_T0_MIN`..`UEB_T9_MIN`) have no ISO 22900-2 ComParam equivalent at
all** — clause 12's own parameter-translation table (Table 37) maps them
only to SAE J2809/J2818's native parameter names, never to a D-PDU
ComParam. **Codex review correction (PR #57): this service has no client-facing
path to any of them at all**, not merely "outside `SetComParam`/
`GetComParam`" as an earlier draft of this ADR claimed — `rpc_misc.rs`'s
`IoCtl` RPC has no generic raw `SET_CONFIG`/`GET_CONFIG` passthrough;
`PDU_IOCTL_GENERIC` is unconditionally unimplemented, and every other
handled IOCTL is a specific, individually-coded D-PDU command (ADR-152
Decision 2's own convention is curated per-command IOCTLs, never generic
forwarding). Every existing `0x8000`-range service-level ComParam this
codebase has ever minted (`CP_CANFD*`, `CP_ChangeSpeed*`) wraps a D-PDU
name ISO 22900-2 already defines somewhere; `UEB_T*` would be the first
case of minting D-PDU vocabulary with no ISO source at all, which is a new
mechanism decision this phase does not make (design-advisor consult, PR
#57 — see Consequences). No `comparam_id.rs`/`service_params.rs` changes
are made this phase; these ten parameters keep whatever default the native
adapter uses, per clause 12.3.4.1's own spec-mandated defaults (Table 36).

**The byte-echo scheme, retransmission, and post-`FIVE_BAUD_INIT` ECU-ID
message handling are entirely the vendor DLL's responsibility.** Clause
12.3.3.1's guidelines consistently describe these as obligations of the
native interface/DLL, not this service — this service's
`events.rs::run_protocol_init`'s five-baud branch already
forwards generically to `PassThru::five_baud_init` and returns whatever
keyword bytes the native call yields, with no ISO9141/KWP-specific
parsing of the response. No change is needed there; this mirrors ADR-165
Repeat Messaging's own "purely a thin IoCtl forwarder" positioning.

## Decision

1. **New `ChannelProtocol::UART_ECHO_BYTE_PS` variant** (`protocol.rs`),
   value `0x0000800A` matching `PROTOCOL_UART_ECHO_BYTE_PS` directly — no
   unqualified base id exists, so (unlike FD/SW/FT-CAN's override pattern)
   the `_PS` id itself *is* the protocol identity, self-mapping through
   `j2534_protocol_id()` the same way `ISO9141`/`J1850PWM` already do.
   `UART_ECHO_BYTE_CHx` Additional Channels are **deferred, matching the
   other eleven ADR-156-out-of-scope families** (GM UART, Honda DIAG-H,
   etc.) — a bare `PROTOCOL_ECHO_BYTE_CH1`/`CH128` header macro already
   exists (`0x9600-0x967F`, unlike FD/SW/FT-CAN which have none) and
   `is_chx_protocol_id`'s numeric range already covers it, so naming it
   directly gets ADR-156's existing generic "recognized but out-of-scope
   `_CHx` family" clean rejection today — the same as every other deferred
   family, not a working resolution. Wiring `UART_ECHO_BYTE_PS` into the
   `chx_block_base`/`chx_protocol_id` `BLOCKS` array is left for a future
   phase if Additional Channels support for this protocol is ever needed
   (edge-case-hunter finding, this phase — an earlier draft of this
   Decision incorrectly claimed the funnel already covered it).
2. **One new resource-table row**, not ten like SWCAN/FT-CAN — those needed
   multiple rows to mirror CAN's own family of D-PDU/OBD-composite
   resources (e.g. `ISO_15031_5_on_ISO_15765_4`); no spec defines an
   equivalent OBD-over-UART-Echo-Byte composite, so a single bare-protocol
   row is correct, mirroring `0x0210 ISO_9141_2 -> ISO9141`'s own
   single-row shape. Next sequential `0x0200`-namespace id (`0x023A`,
   after FT-CAN's `0x0230`-`0x0239`); `hw_protocol_override: None` (this
   row's `protocol` field IS `ChannelProtocol::UART_ECHO_BYTE_PS`
   directly, not an override onto a different base); default pin = J1962
   pin 7 (VW/Audi convention, clause 12.2.2) resolved via the existing
   `resolve_pin_selection`/explicit `SET_CONFIG(CONFIG_J1962_PINS)`
   mechanism (mirroring ADR-164 Decision 1's "always issue explicit
   `SET_CONFIG` even when pins match the row default" pattern — clause
   12.2.2's own statement that no pin is mandated by default for this
   optional protocol, leaving pin assignment to the application, means
   this row's pin is a resolution convenience, not a spec-mandated
   default, same framing already established for SWCAN/FT-CAN);
   `protocol_name` is a
   project-chosen descriptive name (no ISO 22900-2 short name exists to
   alias).
3. **`comparam_support.rs::is_param_allowed` gains an explicit
   exact-identity branch** for `ChannelProtocol::UART_ECHO_BYTE_PS`,
   checked before the "Unknown protocol — allow" fallback, returning
   `is_universal_param(param_id)` only — i.e. `DATA_RATE`/`LOOPBACK`,
   nothing else, matching clause 12.3.4.1's closed parameter list for every
   native `SET_CONFIG`-backed ComParam. This also makes
   `CP_TesterPresentSendType` unreachable for this protocol by
   construction, so mode-0 periodic tester-present (which relies on it)
   can never be armed — satisfying clause 12.3.3.3's exclusion of
   `PassThruStartPeriodicMsg` for this protocol without a separate runtime
   guard at the call site (rejected alternative below).

   `CP_InitializationSettings` (`PARAM_INIT_SETTINGS`, `service_params.rs`
   id `0x8090`) is excluded by this same allowlist too, but NOT because
   clause 12.3.4.1 mandates it: that clause's closed-parameter-list
   restriction is scoped to native `SET_CONFIG`-backed params, and
   `CP_InitializationSettings` has no
   native form at all — it is a D-PDU-only ComParam, resolved entirely
   within this service. The exclusion here is instead a deliberate policy
   choice: Decision 8 below establishes that `FiveBaud` is the only
   spec-valid init outcome for this protocol, so allowing
   `CP_InitializationSettings` would let a caller set a value that carries
   no real information — excluding it avoids introducing a per-protocol
   ComParam value-restriction mechanism (e.g. "settable, but only to `1`")
   solely to express what is already a constant (see Rejected
   alternatives).
4. **`ioctl_start_repeat_message`/`_query_`/`_stop_` (`rpc_misc.rs`, ADR-165)
   gain an explicit `UART_ECHO_BYTE_PS`/`_CHx` rejection**
   (`PDU_ERR_ID_NOT_SUPPORTED`), mirroring the existing software-ISO-TP
   rejection already in that function — clause 12.3.3.1 explicitly
   excludes Repeat Messaging for this protocol (paraphrase: the interface
   cannot reliably track the per-message counter this feature needs
   across an echo-byte exchange), and nothing else in this codebase would
   otherwise prevent a client from attempting it, unlike the
   periodic-tester-present case above.
5. **No service-level enforcement for `PassThruWriteMsgs`'s "exactly one
   message" restriction** (clause 12.3.3.2.2) — verified during
   implementation that this service's existing TX paths (`CoptSendrecv`/
   `CoptStartcomm`/`CoptStopcomm`) never construct a multi-message native
   `PassThruWriteMsgs` call for any protocol; the constraint is satisfied
   by this service's existing single-message-per-call architecture, not a
   new check.
6. **`protocol.rs`'s TX/RX message size-range table gains a
   `UART_ECHO_BYTE_PS`/`_CHx` entry**: TX `4..=256`, RX `3..=256` bytes
   (clause 12.4.2 Table 38) — single addressing mode, no Extended/Normal
   split (K-line-style, matching ISO9141's own entry shape).
7. **Discovery-cache wiring** (device-capability advertisement via the
   `DEVICE_INFO_UART_ECHO_BYTE_*` bits Phase 0 already added) was deferred
   at this phase, matching every prior phase's precedent at the time — later
   superseded by [ADR-185](ADR-185-discovery-cache-connect-time-enforcement.md)
   Stage 1, which wires `DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED` into
   connect-time enforcement (`_SIMULTANEOUS` remains unwired).
8. **`CoptStartcomm`'s `cop_data` must be exactly one byte for this
   protocol** (`rpc_primitive.rs`; design-advisor fix closing two
   edge-case-hunter findings from a second review pass) — clause 12
   defines only 5-baud init (see Context), never a fast-init and never an
   init-less start, so `cop_data` always carries exactly the 5-baud
   address byte and nothing else. A dedicated synchronous guard
   (`link.base_hw_protocol_id() == PROTOCOL_UART_ECHO_BYTE_PS &&
   cop_data.len() != 1`), placed immediately after the `is_kline` binding,
   rejects with `Status::invalid_argument` before init-sequence selection
   runs, closing two failure modes the initial implementation left open:
   empty `cop_data` no longer silently skips protocol init and returns
   success with zero wire traffic; `cop_data.len() >= 4` no longer
   silently misroutes through the legacy heuristic to native `FAST_INIT`,
   which clause 12 never defines. `events.rs::select_init_sequence`'s own
   `legacy_heuristic` closure independently also selects `FiveBaud` for
   this protocol unconditionally now, as defense-in-depth that keeps that
   pure function's own contract correct on its own terms, independent of
   the synchronous guard. Since `CP_InitializationSettings` is unsettable
   for this protocol (Decision 3), the spec-mandated `== Some(1)` path can
   never fire; the legacy single-byte heuristic (now guaranteed exactly
   one byte by the guard above) is the only path that ever resolves an
   init sequence here. The generic five-baud branch in
   `run_protocol_init` itself needed no change — it already forwards
   correctly once an init sequence is actually selected (see Context).

   > **Amended by ADR-183:** the legacy single-byte heuristic, being this
   > protocol's only init path, now also validates and gates on
   > `NumReceiveCycles` (ADR-076's spec-mandated-branch validation, applied
   > here) — see ADR-183 for the full rationale.

### Rejected alternatives

- **Mirroring SWCAN/FT-CAN's ten-row resource-table pattern** — wrong: no
  OBD-family/service-composite spec exists for this protocol to mirror: a
  single bare-protocol row is the correct shape (see Decision 2).
- **Reusing an existing `ChannelProtocol` via `hw_protocol_override`**,
  the FD/SW/FT-CAN pattern — wrong: those three are each spec-framed as
  CAN-family equivalents; UART Echo Byte has no such relationship to any
  existing protocol this service already models (see Context).
- **Leaving the new protocol unclassified in `is_param_allowed`**,
  falling through to "Unknown — allow" — wrong: silently permits every
  ComParam, directly contradicting clause 12.3.4.1's closed parameter
  list (see Context).
- **A runtime `PassThruStartPeriodicMsg`-rejection guard in
  `handle_start_comm`** — unnecessary: the ComParam allowlist exclusion
  (Decision 3) already makes mode-0 periodic tester-present unreachable
  for this protocol, avoiding an extra, easily-forgotten call site that
  would duplicate an already-closed path.
- **Allowing `CP_InitializationSettings` on the UART Echo Byte ComParam
  allowlist** — considered and rejected (Decision 3): the param would
  carry no real information for this protocol, since `FiveBaud` is the
  only spec-valid init outcome (Decision 8); allowing it anyway would mean
  inventing a per-protocol ComParam value-restriction mechanism (accept
  only `1`, reject `2`/`3`) purely to express a constant, rather than just
  keeping the param off the allowlist entirely and letting the `cop_data`
  length guard (Decision 8) carry the actual per-request validation.

## Consequences

- Implements the third resource-table-only J2534-2 phase but the first
  that is genuinely K-line-family rather than CAN-family — establishes
  the "no `hw_protocol_override`, self-mapping `ChannelProtocol` variant"
  pattern for any future standalone (non-CAN-family) J2534-2 protocol
  (e.g. GM UART, Honda DIAG-H, both still open per the plan).
- **Accepted residual (Codex review + design-advisor consult, PR #57):**
  the ten `UEB_T*` timing parameters have no client-facing access path at
  all in this phase — this service has no generic native `SET_CONFIG`/
  `GET_CONFIG` passthrough for a client to reach them through, and minting
  new D-PDU-facing ComParam vocabulary for a native concept ISO 22900-2
  never defined would be this codebase's first case of that shape, a real
  design decision this phase does not make. Every such link runs at
  clause 12.3.4.1's own spec-mandated defaults (Table 36); this is fully
  functional for an ECU on nominal SAE J2809/J2818 timing (the overwhelming
  common case) and only bites an ECU that genuinely needs off-nominal
  5-baud/inter-byte timing, which cannot be accommodated until a future
  phase designs the general mechanism for exposing a native-only, no-D-PDU
  parameter — tracked in `j2534-0404-service/docs/implementation-notes.md`'s
  Prioritized Backlog, first relevant to this protocol but likely shared
  by future phases with the same shape (e.g. GM UART's own clause-11
  parameters).
- Closes clause 12; clause 11's GM UART bus-mastership handshake (`design-advisor:
  Maybe` per the plan's §6 table, not yet implemented) remains open.
  `DEVICE_INFO_UART_ECHO_BYTE_SUPPORTED` discovery-cache connect-time
  enforcement is superseded by [ADR-185](ADR-185-discovery-cache-connect-time-enforcement.md)
  Stage 1 (now consulted at connect time; its `_SIMULTANEOUS` companion bit
  remains unwired by both of ADR-185's stages, tracked in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog).
