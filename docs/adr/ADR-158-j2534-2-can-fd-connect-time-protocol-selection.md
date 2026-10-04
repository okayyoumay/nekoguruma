# ADR-158: SAE J2534-2 CAN FD — Connect-Time Protocol Selection (Phase 3 Stage 3a)

**Date:** 2026-08-04
**Status:** Accepted (narrow supersession of ADR-157 Decision item on `to_j2534_config_id`'s translation contract — see that ADR's Status line; this ADR's PR #30 round-1 correction — rejecting an FD trigger staged on a non-CAN link — is itself narrowly superseded, for the ISO15765 family only, by [ADR-159](ADR-159-j2534-2-iso15765-on-can-fd.md); Decision item 1's `channel_index.is_some()` rejection superseded by [ADR-213](ADR-213-can-fd-additional-channels.md))
**Affects:**
- `j2534-0404/src/lib.rs` (re-exports `PROTOCOL_FD_CAN_PS`, `CONFIG_FD_CAN_DATA_PHASE_RATE`)
- `j2534-0404-service/src/service/resources.rs` (`fd_protocol_id`, `is_fd_protocol_id`,
  `default_pin_select_for_base`, `base_protocol_id`'s new FD arm)
- `j2534-0404-service/src/service/names.rs` (direct FD-id-naming rejection)
- `j2534-0404-service/src/service/comparam_support.rs` (`CP_CANFDTxMaxDataLength` allow-list fix)
- `j2534-0404-service/src/service/comparam_id.rs` (`to_j2534_config_id`'s Plane-A contract,
  `CP_CANFDTxMaxDataLength` range validation)
- `j2534-0404-service/src/service/rpc_link.rs` (`apply_fd_mode`, `connect_new_physical_channel`'s
  data-phase-rate step, `NewPhysicalChannelParams`; `ChannelKey` construction sites widened to a
  4-tuple, PR #30 Correction)
- `j2534-0404-service/src/service.rs` (`ChannelKey` type widened to a 4-tuple, PR #30 Correction)
- `j2534-0404-service/src/service/events.rs` (three `to_j2534_config_id` call sites switched to
  the raw hardware id)
- `j2534-0404-mock/src/lib.rs` (FD_CAN_PS connect/pin-gating/sequencing/read-only-param simulation)
- `j2534-0404-service/tests/grpc_mock/fd_can.rs` (new)
- `j2534-0404-service/src/service/rpc_primitive.rs` (`resolve_send_recv_tx`/`resolve_tester_present`
  FD-aware TX size range, padding, and TX_FD_CAN_FORMAT/TX_FD_CAN_BRS flags, PR #30 round 4
  Correction)
- `j2534-0404/src/lib.rs` (re-exports `TX_FLAG_FD_CAN_FORMAT`/`TX_FLAG_FD_CAN_BRS` as
  `TX_FD_CAN_FORMAT`/`TX_FD_CAN_BRS`, PR #30 round 4 Correction)
- `j2534-0404-service/src/service/names.rs` (`CP_CANFDTxMaxDataLength` shortname mapping, PR #30
  round 4 Correction)

## Context

SAE J2534-2 clause 21 adds CAN FD support to the J2534 v04.04 API. Two facts
drive this design, both confirmed directly against the spec text and the
current code (not re-derived from summary):

1. **Clause 21's Table 89 defines CAN FD purely as a `_PS`/`_CHx` variant —
   there is no unqualified base `PROTOCOL_FD_CAN` id at all**, unlike every
   one of Phase 2's seven `_PS` families (ADR-156 Decision 2), each of which
   narrows from a real base id. `resources::ps_protocol_id`/`base_protocol_id`
   (ADR-156/157) are keyed by base-id round trips; CAN FD has no base id to
   round-trip from, so it needs its own narrow mapping
   (`fd_protocol_id`/`is_fd_protocol_id`), not a seventh entry squeezed into
   the existing `_PS` tables.
2. **ISO 22900-2 (both the 2009(E) and 2022 editions) has no separate CAN-FD
   bus type or resource at all.** CAN FD is expressed purely through
   ComParams (`CP_CANFDBaudrate`, `CP_CANFDTxMaxDataLength`, and the
   already-allow-listed `CP_CANFDBitSamplePoint`/`CP_CANFDSyncJumpWidth`)
   staged on the ordinary `CAN`/`ISO15765` D-PDU resource. A D-PDU client
   never explicitly requests "connect to FD_CAN" the way it explicitly names
   a `_PS`/`_CHx` id for Pin Selection/Additional Channels — the adapter must
   *infer* the native `ProtocolID` from the CLL's staged Working ComParams at
   `ConnectComLogicalLink` time.

Clause 21.3.2.5.1 imposes two further FD-specific hardware rules this design
must satisfy: `CONFIG_FD_CAN_DATA_PHASE_RATE` must be `SET_CONFIG`'d before
`CONFIG_J1962_PINS` on the same channel (else the native connect fails
`ERR_FAILED`), and `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` become read-only
(`SET_CONFIG` → `ERR_NOT_SUPPORTED`) on an FD channel — a distinction Classic
CAN does not have.

**Locked scope for this stage (Phase 3 Stage 3a, "CAN FD core connect
mechanics"):** no Additional Channels (`_CHx`) support for FD_CAN; no
software-ISO-TP engine extension for CAN-FD-sized segmented messages; clause
22 (ISO15765-on-CAN-FD) and clause 8 (Mixed-Format CAN) are separate future
stages (3b/3c), out of scope here.

## Decision

### 1. FD-mode inference is a connect-time substitution, mirroring the
   existing J1850 VPW/PWM auto-detect mechanism's shape

`rpc_link::J2534Service::apply_fd_mode(&self, handle: u32)` runs alongside
`autodetect_sae_j1850_flavor` at the top of `rpc_connect_com_logical_link`,
before that function's own snapshot block reads `hw_protocol_id`/
`base_hw_protocol_override`. It mutates `LogicalLinkState::hw_protocol_id`/
`base_hw_protocol_override` under `logical_links`, and — per ADR-123 Finding
G — calls `recompute_lock_tx_suspensions` in that SAME critical section
before releasing the lock, exactly as the J1850 rewrite does.

**Trigger rule:** `TX_DL > 8 || CP_CANFDBaudrate != 0`, evaluated only when
`resources::base_protocol_id(link.hw_protocol_id) == CAN` (never for
ISO15765 or any other family — clause 22's `FD_ISO15765_PS` is Stage 3b's
job). `TX_DL == 8` alone is Classic CAN's own maximum payload and therefore
ambiguous on its own; a nonzero `CP_CANFDBaudrate` has no purpose except
signaling FD mode and is decisive by itself. The naively "more precise"
framing "`TX_DL > 8` AND a valid baudrate" is actually wrong: `CP_CANFDBaudrate
== 0` is itself a valid, default value (meaning "use `CP_Baudrate`"), so that
framing would never fire for the common case of a caller who sets only
`CP_CANFDTxMaxDataLength`.

**Mode is recomputed fresh on every connect attempt, never sticky across a
disconnect/reconnect.** This mirrors an existing precedent in this codebase:
`connect_flags` (ADR-065) is likewise derived only from the Working
ComParams present at `PassThruConnect` time, so a post-connect
`SetComParam(CP_CanPhysReqFormat, ...)` only takes effect on the *next*
connect, not retroactively on the live channel. A link previously
substituted to `PROTOCOL_FD_CAN_PS` whose Working ComParams no longer
signal FD reverts symmetrically on its next connect: `hw_protocol_id` back
to `CAN` (or `resources::ps_protocol_id(CAN)` if a genuine Pin Selection is
still in effect via `link.pin_select`), `base_hw_protocol_override` cleared
accordingly.

**Rejections** (checked only while actually transitioning INTO FD mode —
`channel_index`/software-ISO-TP mode are immutable for a link's whole
lifetime once created, and module J2534-2 opt-in is static configuration, so
a link that already passed these checks on an earlier connect does not
repeat them on a later reconnect that stays in FD mode):
- Module not opted into J2534-2 (clause 5) — mirrors `resolve_pin_selection`'s
  own opt-in gate.
- `channel_index.is_some()` — FD Additional Channels (`_CHx`) are locked out
  of scope this phase.
- `link.software_isotp` — no software-ISO-TP extension for FD-sized segmented
  messages this phase.

The connecting module's own `pname`/opt-in status is not stored per-CLL;
`apply_fd_mode` instead reads it from `self.device_id`'s currently-open
`module_handle`. This is sound under ADR-107's single-open-device model: a
live `cll_handle` was necessarily created via `CreateComLogicalLink`, which
itself already pinned a device open (`ensure_open_device_for`) for its own
`module_handle` — the same module stays the only one open for the life of
this CLL (barring a concurrent `ModuleDisconnect`, handled conservatively as
"not opted in", the same race window `ensure_open_device`'s own `cll_liveness`
re-check exists to close for its callers).

### 2. `resources.rs` — a dedicated, narrow FD mapping, not a `_PS` table
   entry

`fd_protocol_id(base_hw_protocol_id) -> Option<u32>` (`CAN -> Some
(PROTOCOL_FD_CAN_PS)`, `None` otherwise — room left for `ISO15765 ->
Some(PROTOCOL_FD_ISO15765_PS)` in Stage 3b, not added yet) and
`is_fd_protocol_id(hw) -> bool` are kept separate from `ps_protocol_id`/
`is_ps_protocol_id` (clause 6) rather than folded into those tables, since
CAN FD's own base-id-less shape (fact 1 above) does not fit that table's
round-trip contract, and clause 21's read-only-bit-timing rule (Decision 4
below) has no `_PS` analog to accidentally trigger for.

`base_protocol_id` gains a `PROTOCOL_FD_CAN_PS => CAN` arm, so every
existing Plane B consumer (ADR-157) treats an FD_CAN_PS link as CAN-family
for free — the same reasoning ADR-157 already established for `_PS`/`_CHx`.

`default_pin_select_for_base(base_hw_protocol_id) -> Option<u32>` packs a
base protocol's own default DLC pins (reusing `default_dlc_pins_for_hw_protocol`'s
existing data, not duplicating it) into the same `0x0000PPSS` bitmask
`names::compute_pin_select` produces — needed because an FD_CAN_PS channel
has no "default, pins-preassigned" connect the way Classic CAN's own default
connect does (clause 21 has no base id at all), so even an *unqualified* FD
connect (no `dlc_pin_data` supplied) still needs a real
`SET_CONFIG(CONFIG_J1962_PINS)` — using the base protocol's own conventional
default pins, since the caller expressed no preference.

### 3. Direct FD-id naming is rejected outright, never silently normalized

`names::J2534Service::resolve_pin_selection` — the same choke point Phase
2's `_PS`/`_CHx` direct-naming resolution already lives in — rejects a
`resource_data`/`protocol_id` that numerically names `PROTOCOL_FD_CAN_PS`
directly, unconditionally (before `dlc_pin_data`/opt-in are even consulted),
pointing the caller at the ComParam route instead. This is the same bug
shape ADR-156/ADR-157 repeatedly found and fixed for `_PS`/`_CHx` direct
naming: `base_protocol_id`'s new FD arm (Decision 2) would otherwise
silently normalize a directly-named `PROTOCOL_FD_CAN_PS` down to a plain
`CAN` connect, dropping the caller's FD intent entirely with no error at
all — worse than rejecting, since it fails silently rather than loudly.
Unlike `_PS` (a legitimate direct-naming route when the module is opted in),
there is no legitimate direct-naming route for CAN FD at all — clause 21's
own vocabulary is comparam-driven, not resource-driven — so this rejection
is unconditional, not opt-in-gated.

### 4. `to_j2534_config_id`'s contract becomes Plane-A-keyed, with an FD
   exception — a narrow supersession of ADR-157's "translation uses base"
   rule

ADR-157 established that `to_j2534_config_id`'s per-protocol support matrix
should be keyed by the *base* protocol id (Plane B) — correct for every
family Phase 2 covers, where support never depends on whether the caller
used the base or a `_PS`/`_CHx` variant of it. CAN FD breaks that
uniformity: `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` are supported on Classic
CAN but read-only (clause 21.3.2.5.1) on FD_CAN_PS — a distinction that is
only visible on the raw, un-normalized id, since `base_protocol_id`'s FD arm
collapses `PROTOCOL_FD_CAN_PS` onto `CAN` before the function would ever see
it.

`to_j2534_config_id` now accepts the raw (Plane A) hardware id as its
parameter, checks `resources::is_fd_protocol_id` first to suppress
`BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` (`None`) on an FD link, then normalizes
via `resources::base_protocol_id` for every other param's ordinary
family-arm dispatch — identical behavior to before this ADR for a base id
(`base_protocol_id(base) == base`, ADR-157's own stated invariant) and for
every non-FD `_PS`/`_CHx` id. This is a narrow supersession of ADR-157's
Decision item, not a reversal: the "keyed by base" rule still holds for
every param except the two FD makes read-only, and still holds for every
protocol family except CAN FD.

**Call-site scope, deliberately bounded:** both of `to_j2534_config_id`'s
*direct* callers were updated to pass the raw id — `rpc_link.rs`'s
`apply_j2534_params` (now internally re-deriving the base id for
`expand_tidle`'s own, unrelated ISO9141/ISO14230 family-arm dispatch, so
this change is a no-op for that function) and `events.rs`'s
`apply_params_to_hardware_locked` (same internal re-derivation). Three of
`apply_params_to_hardware`'s own upstream callers (`handle_send_recv`'s and
`handle_start_comm`'s `Temp`-binding hardware pushes, and
`handle_update_param`'s `CoptUpdateparam` push) were switched from an
already-in-scope `base_protocol_id`-named local to its raw sibling
(`protocol_id`/`l.hw_protocol_id`, already present in the same scope for
other reasons), since doing so is a safe, mechanical, zero-risk-for-non-FD
substitution. `revert_hardware_to_live_active`'s 13 call sites and the
RC21/23 timing-change capture path were originally deliberately left passing
the base id — see Consequences. **Superseded: see "Corrections" below —**
**this scope limitation was closed before this PR's initial review; every**
**call site now passes the raw id.**

### 5. Connect-sequencing fix: `FD_CAN_DATA_PHASE_RATE` before
   `CONFIG_J1962_PINS`

`NewPhysicalChannelParams` gains `fd_data_phase_rate: Option<u32>`
(`Some(if CP_CANFDBaudrate != 0 { CP_CANFDBaudrate } else { CP_Baudrate })`
iff FD mode, else `None`), computed in `rpc_connect_com_logical_link`'s
existing snapshot block (after `apply_fd_mode` has already run).
`connect_new_physical_channel` inserts a new, purely conditional step
between the native `PassThruConnect` call and the existing
`CONFIG_J1962_PINS` block: `SET_CONFIG(CONFIG_FD_CAN_DATA_PHASE_RATE, rate)`
when `Some`, with the same disconnect-on-failure rollback shape its
neighboring steps already use. A non-FD connect skips this step entirely —
a conditional insertion, not an unconditional reorder.

The pins step itself is unchanged in shape but now also fires for an
unqualified FD connect (no genuine `link.pin_select`): the computed
`physical_pin_select` local (`link.pin_select.or_else(default packed pins
when FD)`) feeds both the `ChannelKey` tuple's `pin_select` component and
`NewPhysicalChannelParams.pin_select` — the same value in both places, so an
FD channel's `ChannelKey` genuinely reflects the exact pins it will be
assigned, matching how a genuine `_PS` Pin Selection's `ChannelKey` already
works, rather than encoding a `0`-sentinel "no assignment" placeholder for a
channel that in fact always gets one. **`link.pin_select` itself is never
touched** for an unqualified FD connect — it stays `None` — so a later
plain-CAN reconnect on the same link does not incorrectly believe it still
needs to issue `CONFIG_J1962_PINS`.

### 6. Mock simulation

`j2534-0404-mock` simulates `PROTOCOL_FD_CAN_PS` as its own small, separate
concept from `PS_PROTOCOL_IDS` (mirroring the service's own separation,
Decision 2) with three behaviors: accepts `PassThruConnect(FD_CAN_PS, ...)`
with no extra validation (matching the mock's existing "no protocol-id
validation on `PassThruConnect`" convention); starts an FD_CAN_PS channel
`pins_assigned = false` like a `_PS` channel and additionally rejects
`SET_CONFIG(CONFIG_J1962_PINS)` with `ERR_FAILED` until
`CONFIG_FD_CAN_DATA_PHASE_RATE` has already been `SET_CONFIG`'d on that same
channel (clause 21.3.2.5.1's ordering, made testable end-to-end rather than
only unit-testable in isolation); rejects `SET_CONFIG(BIT_SAMPLE_POINT |
SYNC_JUMP_WIDTH)` with `ERR_NOT_SUPPORTED` on an FD_CAN_PS channel. A new
per-channel `set_config_param_log` (mirroring `connect_flags_log`'s existing
len/entry accessor shape) records every `SET_CONFIG` entry's `Parameter` id
in call order, letting integration tests assert the rate-before-pins
ordering directly instead of only inferring it from "connect succeeded."

## Consequences

- **FD vs. Classic links on the same physical pins get distinct
  `ChannelKey`s** (the native `ProtocolID` component alone already differs:
  `PROTOCOL_FD_CAN_PS` vs. `CAN`), so a caller alternating between FD and
  Classic ComParams across reconnects on the same DLC pins opens/closes
  distinct physical channels each time, not one shared channel that changes
  identity in place. A genuine physical wiring conflict between the two
  (both wanting the same J1962 pins simultaneously, on real hardware that
  can't run both at once) surfaces as a native `PassThruConnect`/`SET_CONFIG`
  error, the same way a differing-baud-rate conflict on the same pins does
  today — not specially detected in software.
- **`_CHx` FD (Additional Channels on CAN FD) remains unimplemented** —
  locked out of scope this phase; `apply_fd_mode` rejects the combination
  outright rather than silently ignoring `channel_index`.
- **Software-ISO-TP FD-sized segmentation remains unimplemented** — locked
  out of scope this phase; `apply_fd_mode` rejects the combination outright.
- **FD+UUDT dual-channel-companion interaction: verified structurally
  unreachable, not merely deferred.** `probe_can_channel_mode`'s connect-time
  gate requires `base_proto_id == j2534_0404::ISO15765` exactly, and
  `ensure_uudt_companion_channel`'s gate requires
  `CanChannelMode::applies_to(protocol)` (also `protocol.j2534_protocol_id()
  == ISO15765`). `apply_fd_mode` only ever substitutes a link whose base
  family is `CAN` (never `ISO15765` — that is clause 22/Stage 3b's job, not
  yet implemented), so an FD-substituted link's `protocol`/`base_proto_id`
  can never satisfy either gate's ISO15765 requirement. No "any qualifier
  present" widening (the treatment ADR-156 Decision 3 addendum/ADR-157 gave
  `_PS`/`_CHx` for this same pair of gates) is needed for FD in this phase;
  revisit when Stage 3b adds `FD_ISO15765_PS`, since *that* substitution
  target is exactly the ISO15765 family these gates key on.
- **`revert_hardware_to_live_active`'s 13 call sites and the RC21/23
  timing-change capture path (`events.rs`) still pass the base protocol id
  to `apply_params_to_hardware`, not the raw id** — a deliberate, narrower
  scope than a full call-graph sweep (Decision 4's "deliberately bounded"
  note). **Superseded — see "Corrections" below.** This bullet originally
  argued practical impact was limited (Active can only hold
  `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` via a prior successfully-forwarded
  apply, which FD suppression already prevented) and tracked threading raw
  ids through `revert_hardware_to_live_active`'s 13 call sites as a deferred,
  non-blocking residual. That reasoning was found wrong before this PR's
  initial review — Active legitimately holds never-forwarded params by
  design (ADR-067/068), so an FD-invalid param CAN land there via ordinary
  `SetComParam` + `Connect`/`CoptUpdateparam` — and the sweep was completed
  in the same PR rather than deferred. See the Corrections section for the
  accurate current model.
- Every non-FD link (`is_fd_protocol_id(hw) == false`, true for every link
  that existed before this ADR and every CAN/ISO15765/other-family link this
  ADR itself creates) gets `to_j2534_config_id`/`apply_j2534_params`/
  `apply_fd_mode` behaving identically to pre-ADR-158 code at every fixed
  site — the regression-containment argument, mirroring ADR-157's own.

## Corrections (found before this PR's initial review)

Three issues surfaced during `edge-case-hunter`/`design-advisor` review of
this ADR's initial implementation, before it was ever opened for external
review. All three are fixed in the same PR; none of them changes this ADR's
Decision items 1-3, 5, or 6, or its Consequences bullets other than the two
superseded below.

**1. The unified ComParam-translation pipeline contract.** Decision item 4's
"deliberately bounded call-site scope" reasoning was wrong: it assumed
`apply_params_to_hardware`/`apply_params_to_hardware_locked` (and
`apply_j2534_params`) "internally re-derive the base id for `expand_tidle`'s
own dispatch," but neither of them did — only `to_j2534_config_id`
self-normalized at the time. The corrected design instead makes
`expand_tidle` itself self-normalize the same way `to_j2534_config_id`
already does, so the pipeline now has a single, exceptionless contract:
`to_j2534_config_id`, `expand_tidle`, `apply_params_to_hardware`/
`apply_params_to_hardware_locked`, and `revert_hardware_to_live_active` all
accept the Plane A (raw) hardware protocol id and self-normalize internally
— no caller ever needs to choose which id to pass, and no caller-side
base-id re-derivation exists anywhere in this pipeline. This closes the
exact bug class the next two corrections describe.

**2. The Phase 2 `CP_TIdle` regression this contract gap silently caused.**
Because the "internally re-derives the base id" premise was false, the
three `events.rs` call sites Decision item 4 switched to the raw id
(`handle_send_recv`'s and `handle_start_comm`'s `Temp`-binding pushes, and
`handle_update_param`'s `CoptUpdateparam` push) passed that raw id straight
into `expand_tidle`, which at the time only matched the base `ISO9141`/
`ISO14230` ids exactly — not their `_PS` variants. This silently broke the
already-shipped Phase 2 `CP_TIdle`→`W0`/`W5` derivation (ADR-072) at those
three paths for an `ISO9141_PS`/`ISO14230_PS` link. Fixed by correction 1
above (`expand_tidle` self-normalizing) rather than by any caller-side
change — the existing `TIDLE` tests in `tests/grpc_mock/pin_selection.rs`
never exercised the derived `W0`/`W5` output, only `CP_TIdle`'s own
forwarded value, which is why this shipped undetected; direct unit tests
for the derived output were added alongside the fix.

**3. A corrected accepted-model statement for Active, superseding this
ADR's "Practical impact is limited" Consequences bullet above.** That
bullet's premise — that `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` "can only have
reached Active in the first place via a successful earlier apply" — is
wrong. Per ADR-067/068's Working/Active model, Active is the promoted,
`GetComParam`-visible ComParam set, and it legitimately holds many
never-forwarded params by design; ordinary `SetComParam` followed by
`ConnectComLogicalLink` or `CoptUpdateparam` can populate Active with an
FD-invalid param with no forwarding filter in between. So an FD-invalid
param CAN land in Active on an FD_CAN_PS link. No `SET_CONFIG` carrying it
reaches real hardware today only because `revert_hardware_to_live_active`
unconditionally strips every `PDU_PC_BUSTYPE`-class key before pushing to
hardware (ADR-110's pre-existing boundary) **and** both
currently-relevant FD-read-only params happen to be `PDU_PC_BUSTYPE`-class
— a coincidental, not structural, protection. Because of this, Decision
item 4's "deliberately left passing the base id" choice for
`revert_hardware_to_live_active`'s 13 call sites and the RC21/23
timing-change capture path is corrected to the same raw-id contract as
correction 1, as contract hygiene rather than as a fix to an active bug: a
future FD-invalid param that is not `PDU_PC_BUSTYPE`-class (plausible once
Stage 3b/clause 22 lands) would otherwise silently reach hardware with
nothing to catch it. An integration test pinning both the ADR-110 strip and
this raw-id contract jointly (asserting no `SET_CONFIG(BIT_SAMPLE_POINT)`
ever reaches the mock across a `SetComParam`+`Connect`+`CoptUpdateparam`
promotion+`ParamBinding::Temp` apply/revert bracket sequence on an
FD_CAN_PS link) was added alongside the fix.

None of ADR-067, ADR-068, or ADR-110 is superseded by this correction —
each already stated the model this correction merely applies correctly
here.

## Correction (Codex review, PR #30): `ChannelKey` widened to a 4-tuple for the CAN FD data-phase rate

Codex (P2) found that `ChannelKey` (ADR-156 Decision 2's 3-tuple,
`(hw_protocol_id, baud_rate, pin_select)`) did not include the CAN FD
effective data-phase rate. Two `FD_CAN_PS` links with matching arbitration
`baud_rate`/`pin_select` but different `CP_CANFDBaudrate` staged values
collapsed onto the same `ChannelKey` and silently shared one physical
channel: `connect_new_physical_channel` (which issues
`SET_CONFIG(CONFIG_FD_CAN_DATA_PHASE_RATE)`) only runs for the CLL that
creates a brand-new channel, so a joining CLL's own data-phase rate was
never applied to hardware at all — and a subsequent `CoptUpdateparam` on
the joiner could promote its own `CP_CANFDBaudrate` into its Active set
with no hardware round-trip check, so `GetComParam` could report a rate the
physical channel was not actually running at.

**(a) The fix.** `ChannelKey` is widened to a 4-tuple, appending the link's
effective CAN FD data-phase rate — `CP_CANFDBaudrate` if nonzero, else
`DATA_RATE` (exactly `rpc_link.rs`'s pre-existing `fd_data_phase_rate`
local), `0` for every non-FD link — mirroring exactly how ADR-156 Decision
2 widened `ChannelKey` from a 2-tuple to a 3-tuple to add `pin_select` (the
same "`0` for the common case" convention). The effective data-phase rate
is fixed at connect time exactly like `baud_rate` (the key's second
element) already is: it is immutable, connect-time channel identity, not
genuinely mutable post-connect state, so it belongs in the key itself
rather than in a join-time comparison (the class of check
`client_filters`' join-rejection comparison, and `find_physical_lock_holder`'s
lock-conflict comparison, already handle for state that legitimately
changes after connect).

**(b) The `connect_flags` precedent (ADR-044/065) was considered and
rejected.** `connect_flags` uses a deliberate "creator decides, joiner
inherits" rule and is NOT part of `ChannelKey` — a plausible alternative
would have been to treat the CAN FD data-phase rate the same way. This was
rejected: a `connect_flags` mismatch (e.g. a joiner wanting
`CAN_ID_BOTH` but getting whatever the creator connected with) is
per-message recoverable — each CLL's own UniqueRespIdTable/ComParams still
govern how it builds and interprets messages on the shared channel, so a
mismatched flag degrades gracefully rather than breaking communication
outright. A data-phase-rate mismatch has no equivalent per-message
recovery: the joiner physically cannot transmit or receive at its own
staged rate on a channel actually running at the creator's rate, ever,
for as long as the channel stays open. That distinction — recoverable
per-message state vs. connect-time-fixed physical channel identity with no
recovery path — is why the data-phase rate follows `baud_rate`'s precedent
(in the key) rather than `connect_flags`'s (not in the key).

**(c) The "same-pins FD-vs-Classic" accepted-residual bullet (Consequences,
above) now also covers rate-differing FD links.** That bullet already
states that a genuine physical wiring conflict between two links wanting
the same J1962 pins simultaneously is left to surface as a native
`PassThruConnect`/`SET_CONFIG` error on real hardware, "the same way a
differing-baud-rate conflict on the same pins does today — not specially
detected in software." Two `FD_CAN_PS` links now correctly get distinct
`ChannelKey`s (and so distinct physical channels) when only their
data-phase rate differs; whether opening both simultaneously on
electrically-shared DLC pins is itself viable is exactly the same
real-hardware question the existing bullet already defers, and this fix
adds no new software-side detection for it either — same resolution, same
bullet, not a new one. (This repo's own mock has no cross-channel
pin-exclusivity simulation, so both channels open successfully in this
codebase's test harness regardless; the accepted-residual bullet is a
statement about real-hardware behavior this codebase deliberately does not
attempt to simulate or enforce in software, not a claim the mock itself
rejects the second connect — the regression tests added alongside this fix
assert the actually-observable mock behavior: a second, distinct physical
channel opens, and applies its own data-phase rate.)

**(d) What this fix does NOT change.** A CLL changing `CP_CANFDBaudrate`
post-connect via `CoptUpdateparam` still does not re-open or reconfigure
the physical channel — connect-latched mode remains the documented
residual from Correction 3 above (Active can hold an FD-invalid-for-the-
current-channel param with no hardware round-trip check). This fix only
prevents two DIFFERENT physical channels from being silently collapsed
into one at connect time; it does not add a hardware round-trip check for
an already-connected CLL's later ComParam changes.

## Correction (Codex review, PR #30, declined): CAN FD data-phase rate NOT added to the physical-lock fallback comparison

Codex additionally suggested widening `find_physical_lock_holder`/
`same_physical_resource` (`service.rs`) — the pre-connect fallback used
while either side of a physical-ComParam-lock comparison still lacks a
`channel_key` — to also compare the CAN FD effective data-phase rate,
mirroring how `pin_select` was threaded through this same function for the
`ChannelKey` widening above (and, before that, for the original ADR-157 Bug
D fix). Investigated and declined as a false positive.

**Rationale.** `CP_CANFDBaudrate` is, like `CP_Baudrate`/`DATA_RATE`, a
`BUSTYPE_UNUM32`-class ComParam (`comparam_support.rs`) — exactly the
configuration class `LOCK_PHYSICAL_COM_PARAMS` exists to protect (ISO
22900-2 §9.4.13.3's physical-resource framing). Adding either rate to this
comparison would let a CLL bypass another CLL's physical-ComParam lock
simply by staging a different value for the very parameter class the lock
protects — the opposite of what the check should do. `pin_select` is
different in kind: different pins are different physical wires (a genuinely
distinct bus), so it correctly participates; same pins at a different rate
are the same wires being asked to run a conflicting configuration, which is
exactly the contention this lock exists to prevent. This is also why
`baud_rate` (present in `ChannelKey` since before Phase 2) has never been
part of this fallback comparison — not an oversight, the deliberate other
half of the design `pin_select`'s own threading sits alongside. See
`same_physical_resource`'s doc comment (`service.rs`) for the full
rationale, including the forward-pointing note on this predicate's real,
opposite-direction gap (a false-*negative*, not the false-positive Codex's
finding assumed): an unnormalized FD-vs-Classic `hw_protocol_id` mismatch
and a `None`-vs-synthesized-default `pin_select` mismatch can each let two
CLLs genuinely contending for the same physical wires slip past a lock that
should have caught them. Both are tracked as accepted residuals here, not
fixed by this correction — if this predicate is ever revisited, the correct
direction is making it MORE conservative (normalizing `hw_protocol_id`
through `resources::base_protocol_id`, treating default pins as equal), the
opposite of what the declined finding proposed.

Two regression tests were added to `j2534-0404-service/tests/grpc_mock/fd_can.rs`
pinning the correct behavior in both directions:
`fd_can_lock_conflict_blocks_a_same_pins_different_rate_connect_attempt`
(same explicit pins, differing data-phase rate — still correctly blocked as
a lock conflict) and
`fd_can_lock_does_not_block_a_different_pins_different_rate_connect_attempt`
(genuinely different pins, also differing rate — correctly not blocked,
opening a distinct physical channel).

## Correction (Codex review, PR #30): reject an FD trigger staged on a non-CAN link instead of silently ignoring it

Codex (P2) found that `apply_fd_mode`'s connect-time gate,
`resources::base_protocol_id(link.hw_protocol_id) != CAN` -> `return Ok(())`,
ran *before* `fd_mode` was ever computed. `is_param_allowed`
(`comparam_support.rs`) gates `CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate`
behind `ChannelProtocol::is_can_family()`, which covers `CAN` **and**
`ISO15765` together — so a client can successfully `SetComParam` either
ComParam on a hardware-ISO15765 CLL even though only a `CAN`-family link
ever substitutes to FD. The pre-correction early return meant that staged
FD request was silently dropped: `ConnectComLogicalLink` proceeded as a
classic ISO15765 channel with no error and no other indication the FD
request never took effect.

This directly contradicted Decision item 1's own established pattern for
every other out-of-scope FD combination on a genuinely `CAN`-family link
(`channel_index` set, `software_isotp` mode) — both are rejected outright
(Consequences bullets above: "`apply_fd_mode` rejects the combination
outright rather than silently ignoring"), not silently no-opped. A non-`CAN`
link requesting FD is the same shape of "unsupported this phase" request
and now gets the same treatment: `fd_mode` (`TX_DL > 8 || CP_CANFDBaudrate
!= 0`) is read from the CLL's Working ComParams unconditionally, before the
protocol-family branch, and a non-`CAN` link with `fd_mode == true` returns
`Status::invalid_argument` instead of `Ok(())`. A non-`CAN` link with
`fd_mode == false` (the overwhelmingly common case — no other family's
`is_param_allowed` branch even allow-lists these two ComParams, so this
path is reachable only via the ISO15765 carve-out above) still returns
`Ok(())` exactly as before — no behavior change for any link that never
staged an FD-triggering ComParam.

This does not implement clause 22 (`FD_ISO15765_PS`, Stage 3b) — it only
makes the *absence* of that support loud instead of silent, matching this
ADR's own `_CHx`/software-ISO-TP precedent. `docs/j2534-2-support-plan.md`'s
Stage 3b scope is unchanged.

Two regression tests were added to `j2534-0404-service/tests/grpc_mock/fd_can.rs`:
`fd_can_iso15765_link_with_staged_fd_comparams_is_rejected_not_silently_ignored`
(a hardware-ISO15765 CLL with `CP_CANFDBaudrate` staged on its Working set
gets `PDU_ERR_INVALID_ARG`, `ConnectComLogicalLink` never succeeds) and
`fd_can_iso15765_link_without_fd_comparams_connects_normally` (the ordinary
case — no FD ComParams staged — still connects successfully as classic
ISO15765, confirming the new check adds no false-positive rejection).

## Correction (Codex review, PR #30, round 2): `same_physical_resource`'s `channel_key` branch was rate-sensitive, contradicting its own documented intent

The prior correction's declined-finding reply (above) documented
`same_physical_resource` as rate-insensitive "in either the `channel_key`
branch or the fallback branch." That claim did not hold for the actual
code: the `channel_key` branch (used once BOTH CLLs being compared are
already connected, e.g. by `recompute_lock_tx_suspensions` after every lock
grant/release) compared the raw `ChannelKey` 4-tuple for equality, which
DOES include `baud_rate` and the FD data-phase rate — unlike the fallback
branch, which has always compared only `(hw_protocol_id, pin_select)`. Two
`FD_CAN_PS` CLLs on the same explicit pins and the same arbitration
`baud_rate` but different data-phase rates open two DISTINCT physical
channels by design (`ChannelKey`'s whole reason for a 4th element), so
their `channel_key`s differ — and the old full-tuple comparison read that
as "not the same physical resource," letting a `LOCK_PHYSICAL_COM_PARAMS`/
`LOCK_PHYSICAL_TX_QUEUE` held by one never suspend/block the other, despite
both genuinely contending for the same J1962 pins.

Fixed by extracting only the two physical-identity components
(`hw_protocol_id`, `pin_select` — `ChannelKey`'s elements 0 and 2) from
each side's `channel_key` in this branch, ignoring `baud_rate` and the FD
rate (elements 1 and 3) exactly like the fallback branch already does —
both branches now answer the same rate-insensitive "same wires" question
the doc comment always claimed for both. This is a strict widening (more
resource pairs are now correctly recognized as contending), not a new
gap — the two accepted residuals this same doc comment already tracks (an
FD-vs-Classic `hw_protocol_id` mismatch, and a `None`-vs-synthesized-default
`pin_select` mismatch) are unaffected and remain open.

Two regression tests were added to `j2534-0404-service/tests/grpc_mock/fd_can.rs`:
`fd_can_lock_suspends_a_same_pins_different_rate_sibling_once_both_are_connected`
(two `FD_CAN_PS` CLLs connect to distinct physical channels on the same
pins with different data-phase rates; `cll_a` then locks
`LOCK_PHYSICAL_TX_QUEUE`; `cll_b`'s queued `CoptSendrecv` must stay
suspended until `cll_a` unlocks, proving the `recompute_lock_tx_suspensions`
call site, not just the pre-connect fallback the prior round's declined
finding and its own regression tests already covered) and
`fd_can_lock_resource_rejects_a_same_pins_different_rate_siblings_active_transmission`
(a THIRD `same_physical_resource` call site -- `rpc_lock_resource`'s own
Fix C (ADR-123) active-transmission scan, which runs at grant time rather
than via `recompute_lock_tx_suspensions`: `cll_a`, same pins/different
rate as `cll_b`, has an actively-transmitting COP; `cll_b`'s
`LockResource(LOCK_PHYSICAL_TX_QUEUE)` must be rejected).

## Correction (Codex review, PR #30, round 3): `CoptUpdateparam` could promote a Working snapshot whose FD signal contradicts the already-connected channel's FD-ness

`CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate` have no native `SET_CONFIG`
mapping at all (`ComParamId::to_j2534_config_id`'s `_ => false` catch-all:
"every service-level ComParam ID: no J2534 SET_CONFIG/GET_CONFIG support
for any protocol") — they exist purely as the input `apply_fd_mode` reads
at `ConnectComLogicalLink` time to decide protocol substitution.
`CoptUpdateparam`'s execution path (`handle_update_param`, `events.rs`)
never calls `apply_fd_mode` or reconsiders FD mode at all; it applies the
newly-promoted Working ComParams to hardware using the CLL's
already-connected `hw_protocol_id` and, on success, promotes Working to
Active unconditionally. Since neither FD ComParam has anywhere to forward
to, `CoptUpdateparam` always "succeeds" for them regardless of value.
Concretely: a CLL connects as plain Classic CAN (or hardware ISO15765).
After connecting, the client `SetComParam(CP_CANFDTxMaxDataLength, 64)` --
legal, `is_param_allowed`'s CAN-family gate covers ISO15765 too -- then
runs `CoptUpdateparam`. The COP finishes successfully, promoting Working to
Active. `GetComParam` now reports FD mode requested while the physical
channel hardware is unchanged, genuinely still classic. The mirror-image
direction is equally broken: an FD-connected CLL (`hw_protocol_id ==
PROTOCOL_FD_CAN_PS`, reachable whenever `apply_fd_mode` performed the
connect-time substitution) can promote Working ComParams that no longer
signal FD (`CP_CANFDTxMaxDataLength <= 8` and `CP_CANFDBaudrate == 0`),
making `GetComParam` claim Classic while the channel -- and every
FD-aware consumer still keying off `hw_protocol_id`
(`ChannelKey`/`same_physical_resource`/`to_j2534_config_id`'s
`BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` suppression) -- remains genuinely FD.

`PDU_COPT_UPDATEPARAM` (ISO 22900-2 (2022) Table 14, §8.4.15.9-10) is
defined purely as a Working-to-Active ComParam buffer transfer plus
hardware application; it carries no channel-lifecycle role, so a live
protocol substitution or reconnect from inside this ComPrimitive would
have no basis in the spec and no mechanism in the J2534 adapter either (FD
is not a `SET_CONFIG` knob -- it is the protocol id chosen at
`PassThruConnect`, clause 21; `resources.rs`'s `_PS`/`_CHx` id table is the
only place FD exists as a distinct identity). The only two honest options
are silent divergence (the bug above) or a loud rejection; consistent with
this ADR's own repeated "reject rather than silently ignore" precedent
(the `channel_index`/`software_isotp`/non-`CAN`-family rejections
`apply_fd_mode` already performs), `CoptUpdateparam` now rejects a
promotion whose FD signal (`fd_mode_staged`, the same trigger predicate
`apply_fd_mode` uses, factored out so the two can never drift apart)
disagrees with `resources::is_fd_protocol_id` of the CLL's live
`hw_protocol_id` -- in EITHER direction, symmetrically.

The rejection happens synchronously in `rpc_start_com_primitive`'s
`CoptUpdateparam` branch, at the same instant `update_param_working_snapshot`
already reads the Working ComParam set and UniqueRespIdTable together
(ADR-067 claim A) -- that snapshot helper now also returns the live raw
`hw_protocol_id` from the SAME `logical_links` critical section, so the FD
comparison reads a value from the exact same instant as the params it
judges. This is a call-time check, not an execution-time one inside
`handle_update_param`'s poll task: `hw_protocol_id` cannot change between
call and execution without a disconnect/reconnect, and that path already
bumps `connect_generation`, which `handle_update_param`'s own U1 staleness
check (ADR-086) already cancels this COP for -- a poll-task re-check would
add lock-ordering surface (that function's `api`-before-`logical_links`
discipline, ADR-110's Finding 2 amendment) for no additional coverage, and
could only surface the error via `PduErrEvtProtErr` + a plain
`PduCopstFinished` (asynchronous and indistinct for what is really a
deterministic, synchronously-detectable misuse) rather than a direct RPC
`Status::invalid_argument`, the same surface `apply_fd_mode`'s own
rejections already use.

**Not widened to a within-mode value change.** An FD-connected CLL
promoting a *different* `CP_CANFDBaudrate` value (still nonzero, so
`fd_mode_staged` stays `true`) passes this guard and promotes without any
hardware effect -- the connect-latched data-phase rate is unaffected. This
is the same accepted residual the very first Correction section above
already covers for `DATA_RATE`/ADR-011 (a promoted `CP_Baudrate` value
that differs from the connect-time arbitration rate has never been
forwarded live either); this correction narrows the residual, it does not
close it. Only a signal *crossing* the FD/Classic boundary is rejected.

Three regression tests were added to `j2534-0404-service/tests/grpc_mock/fd_can.rs`:
`coptupdateparam_rejects_promoting_fd_comparams_on_a_classic_connected_link`,
`coptupdateparam_rejects_promoting_classic_comparams_on_an_fd_connected_link`,
and `coptupdateparam_promoting_an_unrelated_param_on_an_fd_connected_link_still_succeeds`
(the within-mode case above, confirming no false-positive rejection).

## Correction (Codex review, PR #30, round 4): FD-connected links could never send an FD-sized (>8 byte) payload

`resolve_send_recv_tx` (`rpc_primitive.rs`) normalizes a link's raw
`hw_protocol_id` through `resources::base_protocol_id` before computing the
TX message size range, so an FD-connected link (`hw_protocol_id ==
PROTOCOL_FD_CAN_PS`) was validated against plain CAN's fixed SAE J2534-1
range (`4..=12`, i.e. 4-byte header + 0..=8 data bytes) regardless of the
link's staged `CP_CANFDTxMaxDataLength`. Every raw CAN payload over 8 data
bytes was rejected before `PassThruWriteMsgs`, even though the FD connect
itself succeeded and `GetComParam` reported FD mode — CAN FD's entire
purpose (payloads up to 64 bytes) was unreachable.

**The fix reads the live staged `CP_CANFDTxMaxDataLength` at call time,
not a fixed 64.** ISO 22900-2's defining semantic for this ComParam is
ISO 15765-2's TX_DL — the tester's own declared max transmit length — so
the cap is this service's obligation to the client's own declaration, and
it must track a live `CoptUpdateparam` promotion within FD mode (the
round-3 guard above permits same-mode `CP_CANFDTxMaxDataLength` value
changes; a fixed 64 would silently ignore a client that deliberately
narrowed it). A new `fd_can_tx_message_size_range` function
(`rpc_link.rs`, beside `fd_mode_staged`) computes `4..=(4 +
effective_tx_dl)`, where `effective_tx_dl` is the staged value floored at
`8` — `CP_CANFDTxMaxDataLength` staged as `0` or `8` alone is Classic
CAN's own ambiguous max (see `fd_mode_staged`'s doc comment) and still
reaches an FD-connected link when only `CP_CANFDBaudrate` is nonzero; ISO
22900-2's own stated fallback for an unset TX_DL is `8`. This is
deliberately a separate function from `ChannelProtocol::tx_message_size_range`
(`protocol.rs`) rather than a new parameter threaded onto it: that
function is the pure SAE J2534-1 per-protocol table with its own lockstep
unit test asserting every row, and the FD bound is link *state* (a live
ComParam), not a protocol constant — threading it through would force
every non-FD caller to carry an unused parameter.

**A payload whose length isn't itself one of SAE J2534-2 Table 91's
DLC-encoded lengths (`0, 1..=8, 12, 16, 20, 24, 32, 48, 64`) is padded up
to the nearest one, not rejected.** ISO 22900-2's `CP_CANFDTxMaxDataLength`
NOTE 5 makes this padding — with `CP_CanFillerByte`, independent of
`CP_CanFillerByteHandling` — the D-PDU API's own duty; rejecting a
41-byte payload as "not a valid FD length" would push a spec-mandated
service responsibility onto the client. A new `fd_can_padded_data_len`
function (`comparam_id.rs`, beside `CANFD_TX_MAX_DATA_LENGTH_ACCEPTED`)
rounds a data length up to the smallest accepted entry `>=` it; the
size-range check above already confirms the unpadded length fits the
link's staged TX_DL, and every accepted entry is `<=` that TX_DL by
construction, so padding never grows the message past the client's own
declared cap.

**The size-range fix alone would still leave every conformant module
rejecting the widened message.** SAE J2534-2 21.4.4 requires a module to
return `ERR_INVALID_MSG` for any TX message outside the Classic `4..=12`
range whose `TX_FD_CAN_FORMAT` flag is unset — this service's TX path
never set that flag anywhere, an omission the mock (which records writes
without enforcing Table 91) made invisible in every test up to this
correction. Both `resolve_send_recv_tx` and `resolve_tester_present` now
OR `TX_FD_CAN_FORMAT` into the resolved TxFlags for any FD-connected
link's message — `resolve_tester_present` gets only the flag, not the
size/padding logic (see the accepted-residual note below for why), and
`TX_FD_CAN_BRS` additionally, iff the connected link staged
a nonzero `CP_CANFDBaudrate` — a zero value means the data phase reuses
the arbitration rate, i.e. no bit-rate switch. This is the same
"objective fact this service already resolved from ComParams, not caller
preference" reasoning ADR-062 already applies to
`TX_FLAG_CAN_29BIT_ID`/`ISO15765_ADDR_TYPE`/`SCI_MODE`/`SCI_TX_VOLTAGE`
just above it in both functions — `TX_FLAG_FD_CAN_FORMAT`/`TX_FLAG_FD_CAN_BRS`
were not previously re-exported from `j2534-0404` at all (added as
`TX_FD_CAN_FORMAT`/`TX_FD_CAN_BRS` in this correction).

**Extended addressing is ignored for the FD range, matching Classic CAN's
own existing behavior** — SAE J2534-2 Table 91 has no AE-byte variant (the
AE byte is an ISO 15765 transport concept, not a raw-CAN one), and clause
21.4.3 fixes the FD header at exactly 4 CAN-ID bytes, same as Classic CAN.

Separately (same review round, mechanical omission rather than a design
gap): `names.rs`'s `resolve_comparam_name`/`map_comparam_name` never
learned the ParamName shortname `CP_CANFDTxMaxDataLength` — unlike its
`CP_CANFDBaudrate`/`CP_CANFDBitSamplePoint`/`CP_CANFDSyncJumpWidth`
siblings, it was only reachable via its raw `0x80BA` id. Added to both the
name-to-id and id-to-name maps; the existing
`map_comparam_name_supports_expected_shortnames` round-trip test now
covers it.

New tests in `j2534-0404-service/tests/grpc_mock/fd_can.rs`:
`fd_can_connected_link_sends_a_full_size_fd_payload`,
`fd_can_connected_link_rejects_a_payload_exceeding_staged_tx_dl`,
`fd_can_connected_link_pads_a_non_dlc_aligned_payload`,
`fd_can_baudrate_only_link_falls_back_to_classic_size_range_but_stays_fd_tagged`,
`fd_can_tx_size_cap_tracks_a_live_coptupdateparam_promotion`, and
`fd_can_tester_present_on_an_fd_connected_link_carries_fd_flags` (added
after an `edge-case-hunter` verification pass on this correction found the
new `resolve_tester_present` flag logic had zero coverage); plus unit
tests `fd_can_tx_message_size_range_tracks_staged_tx_dl_with_an_eight_byte_floor`
(`rpc_link.rs`) and `fd_can_padded_data_len_rounds_up_to_the_nearest_accepted_length`
(`comparam_id.rs`).

**Accepted residuals, not addressed by this correction** (found by the
same `edge-case-hunter` verification pass):
- The RX side was checked (no CAN-specific receive-length assumption
  found — the poll task's only length cap is the generic
  `MAX_MESSAGE_DATA` buffer, well above FD's 68-byte maximum) and needs no
  change.
- `resolve_tester_present`'s `tp_payload` is `CP_TesterPresentMessage`, a
  client-settable Bytefield ComParam that at the time of this correction had
  **no length validation anywhere** in this service, for any protocol — not,
  as an earlier version of this correction's code comment incorrectly
  claimed, a fixed message the service controls. This predated this
  correction (tester-present had never been size-validated) and this
  correction did not regress it; extending FD-only size-validation/padding to
  `resolve_tester_present` here would have closed only the FD slice of that
  gap, not the gap itself. Closed in full (SetComParam-time ISO 22900-2
  `ParamMaxLen`, resolve-time SAE J2534-1 composed-message range including
  the FD/ISO15765-2 single-frame cases this bullet flagged as out of scope)
  by [ADR-215](ADR-215-tester-present-message-length-validation.md).
- `resolve_send_recv_tx`'s other two call sites (`CoptStartcomm`'s
  optional pre-message, `CoptStopcomm`'s final message) were not directly
  exercised by an FD-specific test at first. Traced by hand: both thread
  the same `link.hw_protocol_id`/bound-`params` pairing as the tested
  `CoptSendrecv` call site, protected by the same `connect_generation`
  guard (ADR-086), so no live bug was found — a coverage-gap only, later
  closed by two end-to-end tests (edge-case-hunter, PR #30 round 4):
  `tests/grpc_mock/fd_can.rs::fd_can_connected_link_sends_a_full_size_fd_startcomm_optional_message`
  and `::fd_can_connected_link_sends_a_full_size_fd_stopcomm_final_message`.
- No test exercises `SetComParam` rejecting an out-of-range
  `CP_CANFDTxMaxDataLength` value specifically in combination with
  `fd_can_padded_data_len`'s new `.expect()` invariant (only the
  standalone validator `is_valid_canfd_tx_max_data_length` was
  unit-tested). Added
  `setcomparam_rejects_an_out_of_range_canfd_tx_max_data_length` to lock
  in the guard this `.expect()` depends on.
