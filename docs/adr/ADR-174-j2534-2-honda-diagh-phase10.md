# ADR-174: SAE J2534-2 Honda DIAG-H Protocol (Phase 10)

**Date:** 2026-08-12
**Status:** Accepted (Decision item 1's `HONDA_DIAGH_CHx` deferral fulfilled by ADR-208)
**Affects:** `j2534-0404-service/src/service/protocol.rs`, `resources.rs`,
             `names.rs`, `comparam_support.rs`, `comparam_defaults.rs`,
             `comparam_id.rs`, `rpc_link.rs`, `rpc_primitive.rs`,
             `j2534-0404-mock`

## Context

`docs/j2534-2-support-plan.md`'s Phase 10 covers SAE J2534-2 clause 13, the
Honda DIAG-H protocol: Honda's proprietary "92 Hm/2" diagnostic message
format carried over "DIAG-H," a single-wire, half-duplex, UART-based
physical layer at a fixed 9600bps. New `ProtocolID`s `HONDA_DIAGH_PS`/
`HONDA_DIAGH_CHx`. Phase 0 (ADR-152) already added the raw constants to
`j2534-0404-sys/src/bindings/j2534_v0404.h`: `PROTOCOL_HONDA_DIAGH_PS`
(`0x0000800B`) and the `PROTOCOL_HONDA_DIAGH_CH1`-`CH128` block — no header
edit or bindgen regeneration is needed for this phase.

**Architecturally, this is the second "wholly standalone" J2534-2 protocol,
following ADR-170's UART Echo Byte precedent (Phase 9) rather than the
FD/SW/FT-CAN override pattern.** Clause 13 defines no unqualified base
`ProtocolID` and frames DIAG-H as having no relationship to any protocol
this service already models — the same shape UART Echo Byte occupies, and
the same reasoning ADR-170's Context section already worked through in
detail (not repeated here). A direct search of both available ISO 22900-2
editions for "DIAG-H," "92 Hm," and "Honda" returns zero matches — like UART
Echo Byte, this protocol is vendor-proprietary and ISO 22900-2 defines no
resource for it; a project-chosen `protocol_name` is used instead of an
aliased ISO short name, the same way ADR-170 Decision 2 handled this.

**Pin selection has no default and is a closed set of exactly two single
pins, not one.** Clause 13.2.3/13.2.4 (paraphrase): DIAG-H identifies no
default pin, so the application must explicitly select one; the protocol is
supported on J1962 pin 1 *or* pin 14, and no other pin. This is a new shape
relative to every prior closed-set case this codebase has: SWCAN (clause
9.2.1) closes to exactly one pin, FT-CAN (clause 20.2.1) closes to exactly
two two-pin *pairs*, and DIAG-H closes to exactly two *single* pins — the
single-wire analog of FT-CAN's two-pair set. This is a mechanical extension
of the same allow-list pattern (most recently applied to SWCAN, the "sibling
gap" fix to ADR-168's original FT-CAN check), not a new design question:
`names.rs`'s single-wire branch shape (`is_empty()`/`len() > 1`/else) is
identical to SWCAN's own arm; only the accepted value set widens from one to
two.

**`CoptStartcomm`'s `cop_data` semantics needed a real decision (design-advisor
consult, this phase).** Clause 13.3.1 states the interface performs no
initialization process at all for this protocol — not 5-baud, not fast-init,
nothing; communication begins directly at the fixed baud rate. This is
stronger than UART Echo Byte's own contract (clause 12 still mandates
exactly one 5-baud address byte, ADR-170 Decision 8) — DIAG-H has no init
step for `cop_data` to address at all. `rpc_primitive.rs`'s
`rpc_start_com_primitive` has exactly two established treatments of
`cop_data`, keyed off a single `is_kline` boolean (currently `ISO9141 |
ISO14230 | PROTOCOL_UART_ECHO_BYTE_PS`): `is_kline == true` always consumes
`cop_data` as init-only and never as a message (`tx` stays `None`
unconditionally); `is_kline == false` treats non-empty `cop_data` as a
genuine optional `CoptStartcomm` message (ISO 22900-2 §9.2.6.3.2 b),
resolved via `resolve_send_recv_tx` the same way CAN/J1850/SCI already are.
DIAG-H is physically K-line-like (UART, single-wire) but has no init
consumer for `cop_data` at all, so neither established category applies by
direct analogy without a decision — resolved below.

**A latent ComParam-allowlist gap would otherwise misclassify this protocol,
the same risk ADR-170's Context flagged for UART Echo Byte.**
`comparam_support.rs::is_param_allowed`'s fallback for an unclassified
protocol is "allow everything." Clause 13.3.3.1's closed parameter list
(`LOOPBACK`, `P1_MAX`, `P3_MIN`, `P4_MIN`, `J1962_PINS`) is narrower than
UART Echo Byte's (`DATA_RATE`/`LOOPBACK`): DIAG-H's baud rate is a fixed
9600bps per Table 40, not a `SetComParam`-configurable value at all, so
`DATA_RATE` is *not* on this protocol's client-facing allowlist — the first
J2534-2 protocol phase in this codebase to exclude it there. This is a
client-facing exclusion only: `PassThruConnect`'s native baud-rate argument
is still populated from `ComParamSet::baud_rate()`, which has no
protocol-specific fallback and returns 0 when `DATA_RATE` is absent from the
set — so the connect-time default (`comparam_defaults.rs`) must still seed
`DATA_RATE = 9600` internally for `PassThruConnect`'s own benefit (Decision 4
below), even though `SetComParam`/`GetComParam` continue to reject it. `J1962_PINS` needs no
separate `is_param_allowed` entry: pin selection for every `_PS` protocol in
this codebase flows through `resource_data.dlc_pin_data` at connect time,
never through `SetComParam`, so it was never a `ComParamId` this gate would
see in the first place (the same reason SWCAN/FT-CAN's own allowlists have
no `J1962_PINS` entry either).

**Clause 13 defines no explicit exclusion for Repeat Messaging.** Clause
14.1 states Repeat Messaging (ADR-165) is supported on all protocols unless
a protocol's own clause explicitly excludes it (the mechanism UART Echo
Byte's clause 12.3.3.1 exercises for that protocol). Clause 13's text
(§13.1-13.5) contains no such exclusion for DIAG-H, so the existing generic
`START`/`QUERY`/`STOP_REPEAT_MESSAGE` `IoCtl` forwarding (`rpc_misc.rs`,
resolves `cll_handle -> channel_id -> PassThruIoctl(*_REPEAT_MESSAGE)` with
no protocol-specific gating beyond the software-ISO-TP rejection) already
covers this protocol correctly with no new code — verified directly against
clause 13's text rather than assumed from the UART Echo Byte precedent,
since that precedent went the other way.

## Decision

1. **New `ChannelProtocol::HONDA_DIAGH_PS` variant** (`protocol.rs`), value
   `0x0000800B` matching `PROTOCOL_HONDA_DIAGH_PS` directly — self-mapping
   through `j2534_protocol_id()`, mirroring `UART_ECHO_BYTE_PS`'s own shape
   exactly (Decision 1's reasoning in ADR-170 applies here verbatim).
   `HONDA_DIAGH_CHx` Additional Channels are **deferred**, matching every
   other standalone/CAN-family J2534-2 protocol shipped so far (SWCAN,
   FT-CAN, UART Echo Byte, CAN FD) — the `PROTOCOL_HONDA_DIAGH_CH1`-`CH128`
   header macros already exist and `is_chx_protocol_id`'s numeric range
   already covers them, so naming one directly gets ADR-156's existing
   generic "recognized but out-of-scope `_CHx` family" clean rejection
   today, the same non-resolution every other deferred family gets.
2. **One new resource-table row**, not multiple — clause 13 defines no
   OBD-family composite spec for this protocol to mirror, the same
   reasoning ADR-170 Decision 2 used for UART Echo Byte's single row. Next
   sequential `0x0200`-namespace id; `hw_protocol_override: None` (this
   row's `protocol` field IS `ChannelProtocol::HONDA_DIAGH_PS` directly);
   default pin = J1962 pin 14 — clause 13.2.4's own text singles out pin 14
   as the pin the Honda diagnostic application always targets (including
   for 3-pin/5-pin-DLC vehicles reached through an adapter cable that routes
   pin 14 to the adapter's own pin 1, transparently to the tester
   application), making it the natural resource-table default; this is a
   resolution convenience only, not a spec-mandated default (clause 13.2.3:
   no default pin is identified), the same framing already established for
   SWCAN/FT-CAN/UART Echo Byte's own row defaults. Connect always issues an
   explicit `SET_CONFIG(CONFIG_J1962_PINS)`, matching every prior
   no-true-default protocol's precedent.
3. **`names.rs` gains a new closed-set pin-resolution arm** for
   `HONDA_DIAGH_PS`, positioned after the UART Echo Byte arm (matching the
   existing FD → SW → FT → UART-Echo-Byte ordering — new arms append after
   the last one, dispatch order between sibling arms is otherwise
   immaterial since each is gated on its own distinct `is_*_protocol_id`
   check). Structurally identical to the SWCAN arm (`is_empty()` returns the
   row default only when `resource_row.is_some()`; `len() > 1` rejected,
   single-wire has no secondary pin role); the `else` branch's closed-set
   check accepts exactly `0x0000_0100` (pin 1) or `0x0000_0E00` (pin 14),
   rejecting every other single pin — the two-single-pin analog of the
   SWCAN (one pin) and FT-CAN (two pin-pairs) checks already in this same
   file.
4. **`comparam_support.rs::is_param_allowed` gains an explicit exact-identity
   branch** for `ChannelProtocol::HONDA_DIAGH_PS`, checked in the same
   position as the UART Echo Byte branch, allowing only `LOOPBACK`,
   `P1_MAX`, `P3_MIN`, `P4_MIN` — narrower than `is_universal_param`
   (`DATA_RATE`/`LOOPBACK`), since `DATA_RATE` is excluded per Context
   above. `CP_InitializationSettings` is excluded too, for the same policy
   reason ADR-170 Decision 3 excluded it for UART Echo Byte: Decision 5
   below establishes that no init sequence ever runs for this protocol
   regardless of this param's value, so allowing it would let a caller set
   a value carrying no real information. `comparam_defaults.rs`'s
   `honda_diagh_uart()` nonetheless seeds `DATA_RATE = 9_600` internally —
   this is deliberately not a contradiction of the allowlist exclusion
   above: the seeded value exists solely so `ComParamSet::baud_rate()`
   returns the spec-mandated 9600bps for `PassThruConnect`'s native
   baud-rate argument (`rpc_link.rs`), and stays permanently unreachable via
   `SetComParam`/`GetComParam` because `is_honda_diagh_param` (this
   Decision's branch) never includes `DATA_RATE` in its match. Omitting the
   internal seed entirely (the original draft of this ADR) would silently
   connect at baud_rate=0, risking `ERR_INVALID_BAUDRATE` on real hardware
   that validates the argument — caught in Codex review of PR #63.
5. **`HONDA_DIAGH_PS`/`_CHx` are `is_kline == false`** (design-advisor
   decision) — explicitly *not* added to `rpc_primitive.rs`'s `is_kline`
   match, with a comment at that match documenting the deliberate exclusion
   so a future reader does not "fix" it by grouping DIAG-H with UART Echo
   Byte on physical-layer resemblance alone. A non-empty `CoptStartcomm`
   `cop_data` therefore flows through the existing `resolve_send_recv_tx`
   path as a genuine optional message, exactly as CAN/J1850/SCI already do
   — no new code in `rpc_start_com_primitive` itself. Rationale (full detail
   in the design-advisor consult this ADR records):
   - ISO 22900-2's start-communication taxonomy places a protocol requiring
     no initialization sequence in the "optional message permitted"
     category by default; clause 13's own "no initialization process...
     required" text places DIAG-H squarely there. `is_kline == false` is
     this codebase's existing implementation of exactly that default rule
     — no new mechanism needed.
   - Treating DIAG-H as `is_kline == true` instead would silently discard
     any non-empty `cop_data` with no error (both `five_baud`/`fast_init`
     resolve to `None` regardless of `cop_data` content once no init
     sequence exists to select), a silent-data-loss footgun for a protocol
     whose message structure (clause 13.4.3) otherwise allows a normal
     3-255 byte application-defined message. Worse, a 1-byte `cop_data`
     could let `select_init_sequence`'s legacy heuristic pick `FiveBaud`
     and issue a spec-undefined `FIVE_BAUD_INIT` native call.
   - Rejecting any non-empty `cop_data` outright (a third option considered)
     has no grounding in clause 13's own text and gratuitously diverges
     from the CAN/J1850/SCI treatment for the one protocol family the spec
     most clearly places in the generic "optional message" category.
   - `resolve_send_recv_tx` already applies `ChannelProtocol::tx_message_size_range`
     to whatever it resolves (Decision 6 below gives DIAG-H its own entry),
     so the `CoptStartcomm` optional message gets identical validation and
     framing to a `CoptSendrecv` message — one resolver, one behavior,
     satisfying clause 13.4.4's `DataSize` check synchronously with no new
     code.
   - Contrast with ADR-170 Decision 8: that guard exists because clause 12
     defines a mandatory init that *consumes* `cop_data` as an address byte,
     creating a real ambiguity between "init address" and "message" that
     needed resolving with a dedicated synchronous rejection. DIAG-H has no
     init consumer at all, so that ambiguity does not exist here — the
     generic default applies unopposed, with no analogous guard needed.
6. **`protocol.rs`'s TX/RX message size-range table gains a
   `HONDA_DIAGH_PS`/`_CHx` entry**: TX `3..=255`, RX `1..=255` bytes (clause
   13.4.3 Table 48) — single addressing mode, matching every other K-line
   -style protocol's entry shape (no Extended/Normal split).
7. **Repeat Messaging (ADR-165) needs no new rejection for this protocol**
   (see Context) — the existing generic `START`/`QUERY`/`STOP_REPEAT_MESSAGE`
   forwarding already applies correctly, verified directly against clause
   13's own text rather than assumed.
8. **RxStatus/TxFlags**: clause 13.4.5 states only the `TX_MSG_TYPE`
   RxStatus bit is meaningful for this protocol, and no TxFlags or
   Indications are supported — no code change needed: ADR-098's existing
   5-low-bit `RxFlag` forwarding (which includes `TX_MSG_TYPE`) is already
   protocol-agnostic, and this service constructs no protocol-specific
   TxFlags for a K-line-style message today.
9. **Discovery-cache wiring** was deferred at this phase, matching every
   prior phase's precedent at the time — later superseded by
   [ADR-185](ADR-185-discovery-cache-connect-time-enforcement.md) Stage 1,
   which wires `DEVICE_INFO_HONDA_DIAGH_SUPPORTED` into connect-time
   enforcement (`_SIMULTANEOUS` remains unwired).

### Rejected alternatives

- **Reusing an existing `ChannelProtocol` via `hw_protocol_override`** (the
  FD/SW/FT-CAN pattern) — wrong: clause 13 defines no relationship between
  DIAG-H and any protocol this service already models, the same reasoning
  ADR-170 already rejected this option for UART Echo Byte.
- **`HONDA_DIAGH_PS`/`_CHx` as `is_kline == true`** — wrong: silently
  discards legitimate `CoptStartcomm` messages, and can fire a
  spec-undefined `FIVE_BAUD_INIT` via the legacy heuristic for a 1-byte
  `cop_data` (see Decision 5).
- **Synchronously rejecting any non-empty `CoptStartcomm cop_data`** for
  this protocol — considered and rejected (Decision 5): no basis in clause
  13's text, and gratuitously diverges from the CAN/J1850/SCI treatment
  the generic "no-init protocol" default already prescribes.
- **A single resource-table default pin with no closed-set validation** —
  wrong: clause 13.2.4 documents exactly two valid single pins (1 and 14),
  not an open choice; leaving it unvalidated would repeat the exact SWCAN
  gap this codebase already fixed once for a sibling protocol.
- **Allowing `CP_InitializationSettings` on this protocol's ComParam
  allowlist** — considered and rejected (Decision 4), for the identical
  reason ADR-170 Decision 3 rejected it for UART Echo Byte: the param would
  carry no real information once no init sequence ever runs for this
  protocol regardless of its value.
- **Adding an explicit Repeat Messaging rejection for this protocol** (the
  UART Echo Byte precedent, ADR-170 Decision 4) — considered and rejected:
  clause 13 has no equivalent exclusion text, so applying that precedent
  here would be wrong by direct contradiction of clause 14.1's own default
  rule (see Context).

## Consequences

- Establishes the closed-set pin validation pattern's third instance
  (SWCAN: one pin; FT-CAN: two pin-pairs; DIAG-H: two single pins) and the
  standalone-protocol `ChannelProtocol` pattern's second instance (UART
  Echo Byte, DIAG-H) — both now demonstrated to generalize cleanly across
  more than one J2534-2 protocol.
- Establishes the first J2534-2 K-line-physical-layer protocol classified
  `is_kline == false` in `rpc_primitive.rs` — a precedent for any future
  no-init protocol (e.g. GM UART, clause 11, still open per the plan, if
  its own bus-mastership handshake turns out to need no `CoptStartcomm`-time
  init either; that phase's own reconnaissance must verify this
  independently, since clause 11's handshake is a real protocol behavior
  DIAG-H's clause 13 does not have).
- Closes clause 13. Discovery-cache connect-time wiring for
  `DEVICE_INFO_HONDA_DIAGH_SUPPORTED` is superseded by
  [ADR-185](ADR-185-discovery-cache-connect-time-enforcement.md) Stage 1
  (now consulted at connect time; its `_SIMULTANEOUS` companion bit remains
  unwired by both of ADR-185's stages, tracked in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog).
- **Codex review, PR #63, two rounds (plus an edge-case-hunter follow-up
  within round 2) on `rpc_create_com_logical_link`'s `ComParamSet`
  defaulting.** Round 1 found `honda_diagh_uart()`'s bustype default omitted
  `DATA_RATE` entirely (fixed: seeded internally at 9600bps, Decision 4
  above). Round 2 found the same 0-baud-rate defect reachable through a
  second route: naming the raw `HONDA_DIAGH_PS` id directly with explicit
  `dlc_pin_data` and no `bus_type_name` bypasses `resource_row` entirely, so
  `honda_diagh_uart()` was never selected at all (empty Working set, not
  just a missing key). The edge-case-hunter follow-up then found an initial
  `.or_else`-after-`bustype_name` fix only closed the *absent*-name case —
  `RscData.bus_type`/`.protocol` are independent fields with no
  cross-validation, so a caller could ALSO name `HONDA_DIAGH_PS` alongside
  an unrelated but well-formed `bus_type_name` (e.g. `"ISO_14230_1_UART"`),
  which an after-the-fact fallback never reaches, silently applying that
  other protocol's defaults (10400bps) instead. Fixed by checking
  `resources::is_honda_diagh_protocol_id(protocol.j2534_protocol_id())`
  *before* the `bustype_name` lookup (not as a fallback after it) in the
  `resource_row.is_none()` branch, making the protocol's own fixed identity
  authoritative over whatever `bus_type_name` RscData happens to carry —
  closing both the absent- and mismatched-name cases in one change. The
  identical *absent-bustype-name* gap exists for SWCAN/FT-CAN/UART Echo
  Byte's own connect routes (pre-existing, predates this ADR) — accepted as
  a residual, tracked as a P2 in
  `j2534-0404-service/docs/implementation-notes.md`'s Prioritized Backlog
  rather than generalized here, since a single mechanism covering all four
  protocols is a `design-advisor`-scale decision this round's fix does not
  attempt. **Update: now closed.** `resources::bustype_default_name_for_hw_protocol_id`
  generalizes this same identity-first mechanism to all six standalone `_PS`
  families (SWCAN, FT-CAN, UART Echo Byte, Honda DIAG-H, SAE J1708, SAE
  J1939) — a same-precedent generalization of this fix's own approach, not a
  new design decision, so it carries no new ADR number. The P2 backlog entry
  has been removed.
- **Codex review, PR #63, round 3: `P1_MAX`/`P3_MIN`/`P4_MIN` were accepted
  by `SetComParam` but never reached the native adapter.**
  `comparam_id::ComParamId::to_j2534_config_id`'s shared translation for
  these three params only recognized `ISO9141`/`ISO14230`, and
  `resources::base_protocol_id` does not collapse `HONDA_DIAGH_PS` onto
  either (it self-maps, Decision 1 above) — so `apply_j2534_params` silently
  dropped every staged value (the connect-time default and any client
  override alike) instead of forwarding it via
  `PassThruIoctl(SET_CONFIG)`. The RPC reported success and retained the
  value in link state, but the adapter itself ran at its own unrelated
  timing. Fixed with a `resources::is_honda_diagh_protocol_id`-gated block
  in `to_j2534_config_id`, scoped to exactly `P1_MAX`/`P3_MIN`/`P4_MIN` —
  not the wider `W1`-`W4`/`TIDLE`/`TINIL`/`TWUP`/`PARITY`/`DATA_BITS`/
  `FIVE_BAUD_MOD` group the shared ISO9141/ISO14230 match arm also covers,
  since clause 13's own closed allowlist (Decision 4) never permits those
  others for this protocol. Directly grounded in clause 13.3.1's own "reuses
  ISO9141 timing parameters" text: `CONFIG_P1_MAX`/`_P3_MIN`/`_P4_MIN` are
  J2534-1's generic native timing CONFIG ids, not an ISO9141-specific
  mechanism, so extending the translation to `HONDA_DIAGH_PS` applies that
  clause directly rather than introducing a new one.
- **Codex review, PR #63, round 4: the canonical `protocol_name`
  ("HONDA_DIAGH") route rejected a legitimate non-default pin.**
  `find_table_row_by_name` narrows a name match's `dlc_pin_data` against
  the matched row's own fixed resource-table pin data — a mechanism built
  to disambiguate between several same-named rows (e.g. `SAE_J2610_SCI`'s
  four configurations), not to validate pins for a protocol whose binding
  is resolved dynamically. Row 0x023B has exactly one entry (Decision 2)
  with its own pin-14 convenience default, so naming `"HONDA_DIAGH"` with
  `dlc_pin_data` selecting pin 1 (a documented, valid pin per clause 13.2.4)
  narrowed to zero rows and failed with a generic "matches no
  configuration" error — before `parse_protocol_id_from_resource`'s
  `matched_row_needs_pin_selection` guard (Decision 3) ever routed the
  match through `resolve_pin_selection`'s own correct closed-set check.
  Fixed with a shared `row_needs_dynamic_pin_selection` predicate (the same
  SW/FT/UART-Echo-Byte/Honda-DIAG-H family check `matched_row_needs_pin_selection`
  already used, now factored out and reused by both) that makes
  `find_table_row_by_name` skip fixed-row pin narrowing entirely for a
  single match belonging to one of these dynamically-pin-selectable
  families, deferring to `resolve_pin_selection` downstream instead. As a
  side effect this also closes the identical latent gap for FT-CAN's own
  alternate pin-pair (clause 20.2.1's `(3,11)`, not row 0x0230's own
  `(1,9)` default) and UART Echo Byte's own non-default single pin (clause
  12.2.2 has no closed pin set at all) via their own canonical names —
  neither itself Codex-flagged, but the same mechanism this fix corrects.
  A **close-out edge-case-hunter pass** (after Codex's round-4 approval)
  found this side effect was undertested — only Honda DIAG-H had a
  regression test — and it was closed with two new tests in
  `tests/grpc_mock/ft_can.rs`/`uart_echo_byte.rs` (files this PR does not
  otherwise touch) pinning down the correct, already-shipped behavior so a
  future narrowing of `row_needs_dynamic_pin_selection`'s scope back
  toward "Honda-only" would be caught, not silently reintroduce the bug.
