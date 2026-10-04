# ADR-159: SAE J2534-2 ISO15765-2 on CAN FD — Connect-Time Substitution and ComParam Mapping (Phase 3 Stage 3b)

**Date:** 2026-08-05
**Status:** Accepted (narrow supersession of ADR-158's PR #30 round-1 correction — see that ADR's Status line; Decision item 5's `channel_index.is_some()` rejection superseded, and Decision item 7's `_CHx`-capacity Discovery deferral fulfilled, by [ADR-213](ADR-213-can-fd-additional-channels.md))
**Affects:**
- `j2534-0404/src/lib.rs` (re-exports `PROTOCOL_FD_ISO15765_PS`, `CONFIG_FD_ISO15765_TX_DATA_LENGTH`,
  `CONFIG_N_CR_MAX`, `CONFIG_ISO15765_PAD_VALUE`)
- `j2534-0404-service/src/service/resources.rs` (`fd_protocol_id`/`is_fd_protocol_id`/
  `base_protocol_id` gain an ISO15765 arm)
- `j2534-0404-service/src/service/rpc_link.rs` (`apply_fd_mode`'s ISO15765 rejection becomes a
  substitution; UUDT-companion/probe gates and `install_point_to_point_fc_filters`'s `qualified`
  argument gain an FD disqualifier)
- `j2534-0404-service/src/service/comparam_id.rs` (`to_j2534_config_id` gains its first
  non-identity, protocol-conditional translations; `to_j2534_config_value` gains matching
  conversions)
- `j2534-0404-service/src/service/protocol.rs` (new `fd_iso15765_tx_message_size_range`)
- `j2534-0404-service/src/service/rpc_primitive.rs` (`resolve_send_recv_tx`'s FD branch splits by
  family: CAN keeps 3a's live-TX_DL/padding path, ISO15765 gets a constant range with no padding)
- `j2534-0404-service/src/service/names.rs` (direct FD-id-naming rejection message covers both
  clauses; `CP_Cr` already resolvable, no change needed there)
- `j2534-0404-mock/src/lib.rs` (`FD_ISO15765_PS` connect simulation, mirroring `FD_CAN_PS`'s)
- `j2534-0404-service/tests/grpc_mock/fd_iso15765.rs` (new)
- `j2534-0404-service/docs/implementation-notes.md`, `docs/j2534-0404-architecture.md`,
  `docs/glossary.md`, `docs/j2534-2-support-plan.md`

## Context

SAE J2534-2 clause 22 ("ISO15765-2:2016 on CAN FD") adds a native protocol id,
`FD_ISO15765_PS` (and its `FD_ISO15765_CHx` Additional-Channel siblings), for
running the ISO15765-2 diagnostic transport over a CAN FD physical link
instead of Classic CAN. This is Phase 3 Stage 3b, continuing directly from
[ADR-158](ADR-158-j2534-2-can-fd-connect-time-protocol-selection.md) (Stage
3a, clause 21's plain CAN FD) per the sub-staging `docs/j2534-2-support-plan.md`
already committed to (ADR-155: 3a/3b/3c are separate PRs).

Three facts, confirmed directly against the spec text and current code, drive
this design:

1. **Clause 22's Table 96 defines `FD_ISO15765_PS`/`FD_ISO15765_CHx` the same
   way clause 21 defined `FD_CAN_PS`: `_PS`/`_CHx`-only, no unqualified base
   id.** ISO 22900-2 (2009(E) and 2022 editions both checked) has zero
   knowledge of clause 22 — no separate resource, no clause-22-specific
   ComParam. Exactly like clause 21, a D-PDU client never explicitly
   "connects to FD_ISO15765" — the adapter must infer it.
2. **Unlike clause 21, ISO 22900-2 already owns every tuning concept clause
   22 exposes as a native `SET_CONFIG` parameter.** Table 97's five new
   native params are `FD_CAN_DATA_PHASE_RATE` (already implemented by
   ADR-158, reused unchanged), `FD_ISO15765_TX_DATA_LENGTH` (the data-phase
   frame size for single/first/consecutive frames — the same concept
   `CP_CANFDTxMaxDataLength` already stages), `N_CR_MAX` (consecutive-frame
   timeout — the same concept `CP_Cr`, ISO 22900-2's existing ComParam,
   already stages), `ISO15765_PAD_VALUE` (the same concept `CP_CanFillerByte`
   already stages), and `HS_CAN_TERMINATION` (bus termination — deferred,
   see Decision 6). Clause 22 therefore needs **no new ComParam** for these
   three, but it does need `to_j2534_config_id` to learn its first
   **non-identity** translation: on an `FD_ISO15765_PS` link, these three
   existing ComParams must forward to *different* native `CONFIG_*` ids than
   the ids they already share names with on every other protocol.
3. **The adapter runs the real ISO15765 state machine on this link, not this
   service.** Unlike clause 21's raw-CAN `FD_CAN_PS` (where this service
   builds every single/first/consecutive frame itself, hence Stage 3a round
   4's size-range/padding/flag logic in `resolve_send_recv_tx`), a hardware
   `FD_ISO15765_PS` connection hands the service's already-assembled logical
   message straight to `PassThruWriteMsgs` and lets the native adapter
   segment, pad, and manage flow control — the same division of labor
   Classic hardware ISO15765 already has today. Stage 3b therefore needs a
   size-range **widening**, not Stage 3a's padding/DLC-rounding machinery.

Clause 22's own connect-sequencing rule (spec ~line 3051) is identical to
clause 21.3.2.5.1's: `FD_CAN_DATA_PHASE_RATE` must be `SET_CONFIG`'d before
pin selection, else `ERR_FAILED`. ADR-158's `connect_new_physical_channel`
step already implements this generically (keyed on `fd_data_phase_rate:
Option<u32>`, not on which FD family triggered it), so it needs no change.

**A gap ADR-158 explicitly flagged for this stage:** ADR-158's own
Consequences section noted that Stage 3a's UUDT-companion/dual-channel
machinery (`rpc_link.rs`'s companion-channel gates and
`install_point_to_point_fc_filters`) keys on "base family is ISO15765, no
`pin_select`/`channel_index`" — properties an FD-substituted ISO15765 link
also satisfies. Without a fix, Stage 3b would silently open a **Classic-CAN**
UUDT companion channel alongside an FD-connected primary, and let an FD link
participate in the module-wide CAN-mode probe as if it were an ordinary
Classic link. This ADR closes that gap.

**Locked scope for this stage:** no `FD_ISO15765_CHx` (Additional Channels)
support; no clause-8 (Mixed-Format CAN) connect-flag support — clause 22's
own device-level mixed-format requirements (22.2.2.a/d/g/i/j: sending and
receiving both CAN 2.0- and FD-format ISO15765 frames on one link, echoing
flow control in whichever format triggered it, accepting flow control in
either format, NACKing an oversized first frame) are native-adapter
obligations this service takes on zero implementation burden for, since it
never constructs frames on a hardware ISO15765 link; `HS_CAN_TERMINATION` and
discovery wiring for `FD_ISO15765_PS` are deferred (Decisions 6-7).

## Decision

### 1. Connect-time substitution reuses `fd_mode_staged` unchanged, widened to the ISO15765 family

`apply_fd_mode`'s existing non-CAN-family branch currently rejects outright
whenever `fd_mode_staged` is true (ADR-158's PR #30 round-1 correction,
whose own commit message says "until FD_ISO15765_PS support is
implemented" — this is that implementation). It becomes a family dispatch
on `resources::base_protocol_id(link.hw_protocol_id)`:

- `CAN` — unchanged (ADR-158's existing substitution logic).
- `ISO15765` — a new branch mirroring the `CAN` branch exactly: reject if
  not opted into J2534-2 (clause 5) or if `link.channel_index.is_some()`
  (Additional Channels FD out of scope); otherwise substitute
  `link.hw_protocol_id = fd_protocol_id(ISO15765)` /
  `base_hw_protocol_override = Some(ISO15765)` when entering FD mode, and
  revert symmetrically (via `ps_protocol_id(ISO15765)` when `pin_select`
  is set, else plain `ISO15765`) when leaving it — the same "recomputed
  fresh on every connect, never sticky" rule ADR-158 established.
  `link.software_isotp` needs no live check in this branch: a
  software-ISO-TP CLL's `hw_protocol_id` is always raw `CAN` regardless of
  service-level protocol identity (`can_mode.rs`), so it is caught by the
  existing `CAN`-branch software-ISO-TP rejection instead — a
  `debug_assert!(!link.software_isotp)` documents this instead of a
  redundant runtime check.
- Any other family with `fd_mode` staged — still rejected outright (error
  message updated to say clause 21/22 are the only two FD-supporting
  families).

`fd_mode_staged`'s trigger predicate itself (`CP_CANFDTxMaxDataLength > 8 ||
CP_CANFDBaudrate != 0`) is unchanged and applies identically on ISO15765:
ISO 22900-2 has no clause-22 vocabulary of its own (fact 1 above), and
`is_param_allowed`'s CAN-family gate already admits both trigger ComParams
on ISO15765 — the round-1 correction's own premise for rejecting them was
exactly that a client *could* stage them there.

`resources.rs` gains the mirror-image mapping:

```
fd_protocol_id(ISO15765)  = Some(PROTOCOL_FD_ISO15765_PS)
is_fd_protocol_id(PROTOCOL_FD_ISO15765_PS) = true
base_protocol_id(PROTOCOL_FD_ISO15765_PS)  = ISO15765
```

Every existing `is_fd_protocol_id`/`base_protocol_id` call site this widens
for free (audited individually, not assumed): the connect-time snapshot
block's default-pins/data-phase-rate/`ChannelKey` logic (ADR-158's
`fd_data_phase_rate` step and 4-tuple `ChannelKey` are keyed on FD-ness
generically, not on which base family), the round-3 `CoptUpdateparam` FD-mode
guard (`fd_mode_staged` vs. `is_fd_protocol_id`), `names.rs`'s direct-`_PS`
naming rejection, and `resolve_tester_present`'s FD-flag logic (Stage 3a
round 4). `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` becoming read-only on an FD
channel (clause 22.3.2.6.1, same rule as clause 21.3.2.5.1) comes free
through the same widened `is_fd_protocol_id` gate `to_j2534_config_id`
already checks first.

### 2. `to_j2534_config_id` gains its first non-identity, protocol-conditional translations

Before this ADR, `to_j2534_config_id` only ever returns `Some(self.0)`
(identity) or `None` — every native CONFIG id a ComParam maps to is the same
numeric id as the ComParam itself, a fact `comparam_id.rs`'s own doc comment
relies on. Clause 22 breaks this: on an `FD_ISO15765_PS` link specifically
(checked against the raw, un-normalized `hw_protocol_id`, mirroring the
existing FD-vs-`BIT_SAMPLE_POINT` check's ordering), three ComParams that are
either unsupported or identity-mapped everywhere else gain a **different**
native target:

| ComParam | Everywhere else | On `FD_ISO15765_PS` |
|---|---|---|
| `CP_CANFDTxMaxDataLength` (`PARAM_CANFD_TX_MAX_DATA_LENGTH`) | unsupported (pure trigger signal, no native mapping — ADR-158 round 3/4) | `CONFIG_FD_ISO15765_TX_DATA_LENGTH` |
| `CP_Cr` (`PARAM_N_CR`) | unsupported (only consumed by the unrelated software-ISO-TP `Reassembly` timeout, ADR-046) | `CONFIG_N_CR_MAX` |
| `CP_CanFillerByte` (`PARAM_CAN_FILLER_BYTE`) | unsupported for native forwarding (only consumed by ISO15765-2 software frame padding) | `CONFIG_ISO15765_PAD_VALUE` |

`to_j2534_config_value` gains matching unit conversions for these three
targets:

- `CONFIG_FD_ISO15765_TX_DATA_LENGTH`: `value.max(8)` — a staged `0`
  (unset) floors to `8`, the same floor `fd_mode_staged`'s own doc comment
  and Stage 3a's `fd_can_tx_message_size_range` already establish for an
  unset TX_DL, rather than letting Table 97's native default (`64`) silently
  win over an explicit-but-small staged value.
- `CONFIG_N_CR_MAX`: `us_to_ms(value).clamp(1, 0xFFFF)` — `CP_Cr` is
  µs-resolution (ISO 22900-2, default `1_000_000` µs = 1000 ms across every
  seeded preset), Table 97's native range is `$0001`-`$FFFF` ms.
- `CONFIG_ISO15765_PAD_VALUE`: identity — both sides are a plain `0..=0xFF`
  byte value.

**`CP_CANFDBaudrate` stays exactly as ADR-158 left it** (no native mapping;
`FD_CAN_DATA_PHASE_RATE` is set once, at connect time, from the
already-existing `fd_data_phase_rate` connect step — clause 22 reuses that
mechanism unchanged, it does not need a second one).

**Consequence for the round-3 `CoptUpdateparam` residual:** ADR-158 round 3
recorded that an FD-connected link promoting a same-mode ComParam value (e.g.
a different but still-nonzero `CP_CANFDBaudrate`) has no live hardware
effect, since nothing forwards it post-connect. On an `FD_ISO15765_PS` link
specifically, this residual **narrows**: `CP_CANFDTxMaxDataLength`/`CP_Cr`/
`CP_CanFillerByte` now go through the ordinary `apply_params_to_hardware`
pipeline like any other native-mapped ComParam, so a live `CoptUpdateparam`
promoting any of them genuinely reaches the adapter. `CP_CANFDBaudrate`
itself remains connect-latched, unchanged.

### 3. Message-size validation: a constant range, no service-side padding

New `protocol.rs` function `fd_iso15765_tx_message_size_range(extended:
bool) -> RangeInclusive<usize>` = `4..=4128` (normal) / `5..=4128` (extended
addressing) — Table 98's numbers directly (4/5-byte header + up to
4124/4123 payload bytes, capped by the shared `4128` maximum both rows
share). Placed beside `fd_can_tx_message_size_range` (Stage 3a round 4) with
its own lockstep unit test, not folded into `ChannelProtocol::tx_message_size_range`
for the same reason Stage 3a's function isn't: that table is a pure SAE
J2534-1 per-protocol constant with its own asserted-every-row test, and FD-ness
is link state, not a protocol constant.

In `resolve_send_recv_tx`, the existing `fd_link` branch (Stage 3a round 4)
splits by base family instead of treating every FD link identically:

- **FD + CAN** (`FD_CAN_PS`): unchanged from Stage 3a — live-staged-TX_DL
  range from `fd_can_tx_message_size_range`, DLC-rounding padding via
  `fd_can_padded_data_len`, because this service constructs every raw CAN FD
  frame itself.
- **FD + ISO15765** (`FD_ISO15765_PS`): the new constant
  `fd_iso15765_tx_message_size_range` range, **no padding** — padding
  individual ISO15765 frames (single/first/consecutive, both CAN 2.0- and
  FD-format per clause 22.2.2.e/f) is the native adapter's own ISO15765-2
  duty, driven by the client's `TX_ISO15765_FRAME_PAD` flag and the
  newly-forwarded `CONFIG_ISO15765_PAD_VALUE`, exactly as it already is for
  Classic hardware ISO15765 today.

Both FD cases keep Stage 3a round 4's `TX_FD_CAN_FORMAT` (+`TX_FD_CAN_BRS`
iff a nonzero `CP_CANFDBaudrate` is staged) flags — Tables 99/100 apply these
flags to every ISO15765-on-CAN-FD message exactly as they do to plain FD
CAN messages, the same objective-fact reasoning ADR-062/Stage-3a-round-4
already established.

RX needs no change: FD_ISO15765_PS's max RX size (`4128`, Table 98) already
equals `MAX_MESSAGE_DATA`, the buffer cap every RX path already respects.

**Deferred, documented as an accepted residual:** the ADR-055 ISO15765
functional-addressing Single-Frame limit (`isotp::Addressing::max_sf_payload`,
currently `7`/`6` bytes, sized for Classic CAN's 8-byte frame) is not widened
for an FD-connected link's larger frame capacity in this stage. A functional
request on an `FD_ISO15765_PS` link is therefore more conservative than the
spec strictly requires (it rejects an SF payload the FD frame size could
actually carry), never less — a client can still use physical addressing to
reach the same result. Widening it correctly requires deriving the FD SF PCI
byte-count difference (ISO 15765-2:2016's two-byte FD SF header vs. Classic's
one-byte header) from the live staged TX_DL, which is a distinct, self-contained
follow-up not required for this stage's core connect/transmit path to work
correctly.

### 4. UUDT-companion and probe machinery treat FD-ness as a third disqualifier

The gap ADR-158's Consequences section flagged: `rpc_link.rs`'s two
UUDT-companion-channel gates and the module-wide `probe_can_channel_mode`
gate currently key on "base family is ISO15765, `pin_select.is_none()`,
`channel_index.is_none()`" — properties an FD-substituted ISO15765 link also
satisfies, since FD substitution happens independently of Pin
Selection/Additional Channels. Each of the three gates gains `&&
!resources::is_fd_protocol_id(link.hw_protocol_id)` (or the raw id
equivalent already in scope at that call site).

**Mandatory lockstep, not independent edits:** `install_point_to_point_fc_filters`'s
`qualified` argument (its own doc comment already warns that letting this
argument drift from the companion gates it mirrors reproduces a prior
silent-UUDT-loss bug) must gain the exact same `|| is_fd_protocol_id(...)`
disqualifier at all three of its call sites, in the same commit as the companion-gate
changes above — an FD-connected ISO15765 link must get the point-to-point
fallback filter (since it genuinely can receive unsolicited UUDT traffic on
its own single channel, both CAN-2.0- and FD-format per clause 22.2.2.d) in
place of the Classic-format companion channel it would otherwise wrongly
open.

### 5. Scope boundary: `FD_ISO15765_CHx` and clause 8 stay out

`FD_ISO15765_CHx` (128 Additional-Channel variants) is rejected on both
routes into a connect: direct naming falls inside the existing clause-24
`_CHx`-region check (`PROTOCOL_FD_ISO15765_CH128` is already that region's
upper bound), and the ComParam-inference route hits `apply_fd_mode`'s
existing `channel_index.is_some()` rejection (Decision 1).

Clause 8 (Mixed-Format CAN, `CAN_MIXED_FORMAT`) remains fully Stage 3c's
scope. Clause 22's own device-level mixed-format requirements (22.2.2.a/d/g/i/j)
are native-adapter obligations this service does zero implementation work
for, since a hardware `FD_ISO15765_PS` link never has this service
constructing its frames — Stage 3b sets no clause-8 connect flag and adds no
clause-8 groundwork.

Software-ISO-TP combined with FD stays rejected via the existing (unchanged)
`CAN`-branch check in `apply_fd_mode` (Decision 1's `debug_assert`).

### 6. `HS_CAN_TERMINATION`: deferred

`CP_TerminationType` (ISO 22900-2, values `0..4`) could map cleanly for its
`0`/`3` values (no termination / 120 Ω, matching Table 97's two valid native
values), but this ComParam is "stored, not forwarded to any native CONFIG"
for every protocol today, values `1`/`2`/`4` have no native encoding to fall
back to, and nothing in clause 22's connect sequencing depends on bus
termination. Wiring only the FD_ISO15765_PS case creates an inconsistency
wider than the value it adds. Recorded as an `implementation-notes.md`
backlog item rather than fixed here.

### 7. Discovery: deferred, matching Stage 3a's precedent

Stage 3a wired no `DEVICE_INFO_FD_CAN_*`-equivalent discovery-cache
consumption at all (`apply_fd_mode` only checks the module's clause-5 pname
opt-in, never a discovery-reported capability bit) — consistent with
ADR-153's explicit "no connect-time enforcement wiring yet" deferral for
earlier phases. Stage 3b defers identically for `DEVICE_INFO_FD_ISO15765_SUPPORTED`/
`_SIMULTANEOUS`/`_PS_J1962`: an adapter that doesn't actually support this
feature still gets a normal connect attempt, which fails with
`ERR_NOT_SUPPORTED` per clause 22.3.2.1 — already a correctly-surfaced
connect error, just not a pre-emptive one.

### 8. RX flag surfacing: non-issue

This service forwards only `RX_STATUS_FLAGS_MASK`'s five low RxStatus bits
into `ResultData.rx_flag` (ADR-098) — `FD_CAN_BRS`/`FD_CAN_FORMAT`/`FD_CAN_ESI`
(bits 19-21) are masked out today, and ISO 22900-2's RxFlag vocabulary has no
FD-related bits to map them onto in the first place (it has no FD concept at
all, per this and ADR-158's Context). No re-export, no service change; an
accepted residual, not a gap this stage introduces.

## Consequences

- **`to_j2534_config_id`'s implicit "always identity" contract is now
  false** for three ComParams on one specific hardware id. The function's
  own doc comment and its unit test suite must both be updated to assert
  this explicitly (an identity-mapping regression test for every pre-159
  arm, plus the three new non-identity cases), so a future refactor doesn't
  silently reintroduce a bare `Some(self.0)`.
- **The round-3 `CoptUpdateparam` residual narrows on `FD_ISO15765_PS`
  links** (Decision 2) — `CP_CANFDTxMaxDataLength`/`CP_Cr`/`CP_CanFillerByte`
  promotions now have genuine live hardware effect where they previously had
  none on any FD-connected link. `CP_CANFDBaudrate` remains connect-latched
  on both FD families, unchanged.
- **`same_physical_resource`'s existing FD-vs-Classic accepted residual**
  (ADR-158's declined lock-scope finding) now spans the ISO15765 family too,
  in the same already-accepted direction (too permissive, never too strict) —
  no new gap, just a wider blast radius for the same documented one.
- **The ADR-055 functional-SF-limit conservatism** (Decision 3) means a
  functional request on an FD-connected ISO15765 link may reject a payload
  the frame size could actually carry; physical addressing is unaffected.
  Recorded as a follow-up, not blocking. **Resolved by ADR-169**, which
  widens the limit for a native `FD_ISO15765_PS` link based on its staged
  `CP_CANFDTxMaxDataLength`.
- ~~**A pre-existing, unrelated bug was found and deliberately NOT fixed
  here:** `ComParamSet::isotp_n_cr_timeout_ms`/`isotp_n_bs_timeout_ms`
  (software-ISO-TP's own N_Cr/N_Bs timeout resolution, ADR-046) read
  `CP_Cr`/`CP_N_Bs`'s µs-resolution stored values and return them unconverted
  as if they were already milliseconds — a ~1000x timeout inflation for the
  unrelated software-ISO-TP emulation path. This is orthogonal to Stage 3b's
  native-hardware `CONFIG_N_CR_MAX` forwarding (Decision 2), which reads the
  same raw stored µs value directly and converts it correctly via
  `us_to_ms`. Fixing the software-ISO-TP helpers' unit bug is out of scope
  for this PR (it would silently change unrelated ADR-046 behavior) —
  recorded as an `implementation-notes.md` backlog item.~~ **FIXED (2026-08-07):**
  both helpers now convert via the same `us.div_ceil(1000).max(1)` pattern
  `p2_max_timeout_ms` already used, mirroring the correct conversion
  Decision 2's `CONFIG_N_CR_MAX` forwarding already applied via `us_to_ms`.
  A standalone fix, unrelated to this ADR's own scope.
- **`HS_CAN_TERMINATION` and `FD_ISO15765_PS` discovery wiring remain
  unimplemented**, consistent with existing precedent (Decisions 6-7) —
  both recorded as backlog items, not silent gaps.
- Clause 8 (Mixed-Format CAN) and `FD_ISO15765_CHx` (Additional Channels)
  remain fully out of scope, unblocking future Stage 3c work without any
  Stage 3b assumption to unwind.
