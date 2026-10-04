# ADR-169: Widen the Functional-Addressing Single Frame Limit for FD_ISO15765_PS

**Date:** 2026-08-11
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/isotp.rs`,
             `j2534-0404-service/src/service/rpc_link.rs`,
             `j2534-0404-service/src/service/rpc_primitive.rs`

## Context

ADR-055 rejects a functionally addressed ISO15765 request whose `cop_data`
exceeds `isotp::Addressing::max_sf_payload()` — a fixed 7 bytes under Normal
addressing, 6 under Extended, sized for Classic CAN's 8-byte frame — because
a broadcast request has no specific target to negotiate FlowControl with for
a multi-frame exchange. That check runs uniformly for every ISO15765 link,
keyed off `protocol.j2534_protocol_id() == ISO15765` (service-level
identity), including a native `FD_ISO15765_PS` link connected via ADR-159,
where the real adapter runs the actual ISO15765 state machine with genuine
CAN FD frame capacity. ADR-159's own Decision 3/Consequences section
recorded this as a known conservatism: a functional request on an
FD-connected link may be rejected even though the link's staged
`CP_CANFDTxMaxDataLength` (`PARAM_CANFD_TX_MAX_DATA_LENGTH`, native SAE
J2534-2 `CONFIG_FD_ISO15765_TX_DATA_LENGTH`) could actually carry it — this
phase closes that follow-up.

`CP_CANFDTxMaxDataLength` is already validated and staged as one of
`[0, 8, 12, 16, 20, 24, 32, 48, 64]` (`comparam_id.rs`'s
`CANFD_TX_MAX_DATA_LENGTH_ACCEPTED`, SAE J2534-2 Table 90/97) — `0`/unset
means Classic CAN framing. `rpc_link.rs`'s `fd_can_tx_message_size_range`
already treats an unset/`0` staged value as `8` (Classic CAN's own frame
size) for its own FD size-range computation; this phase reuses that same
fallback rule.

**Spec-availability note:** unlike ISO 22900-2/SAE J2534-1/SAE J2534-2, the
ISO 15765-2:2016 text is not present in the sibling `vehicle-comm-specs`
repository this workspace normally cites against. The CAN FD Single Frame
framing rule below is derived from well-established, publicly documented
ISO-TP engineering knowledge (the same mechanism implemented by, e.g., the
Linux kernel's `can-isotp` driver and other public ISO-TP stacks), not
transcribed from the normative text, and is cited here without a clause
number for that reason. Confidence is high on the escape-sequence mechanism
and the resulting size formula; if this is ever found to disagree with an
authoritative copy of the spec, revisit this ADR rather than assuming the
formula below is exact in every edge case.

## Decision

1. **CAN FD Single Frame framing rule.** When an ISO15765 frame's actual
   data length exceeds 8 bytes (i.e. a CAN FD frame, not Classic CAN), the
   Single Frame PCI switches from the Classic one-byte form (high nibble
   `0x0`, low nibble carries the length, capacity limited by the nibble's
   4 bits but physically capped at 7/6 bytes by the 8-byte frame) to a
   2-byte escape form: PCI byte `0x00` followed by a full second byte
   carrying the actual SF payload length. This costs one extra header byte
   versus the Classic form. The Address Extension byte (Extended addressing)
   reduces the result by the same flat 1 byte the Classic form already
   subtracts — the escape trigger is keyed on the frame's own data length,
   not on addressing mode, so there is no addressing-specific threshold
   difference. This yields, per staged `CP_CANFDTxMaxDataLength` (`0`/unset
   treated as `8`):

   | TX_DL | Normal max SF | Extended max SF |
   |---|---|---|
   | 0 / unset / 8 | 7 | 6 |
   | 12 | 10 | 9 |
   | 16 | 14 | 13 |
   | 20 | 18 | 17 |
   | 24 | 22 | 21 |
   | 32 | 30 | 29 |
   | 48 | 46 | 45 |
   | 64 | 62 | 61 |

2. **New `Addressing::fd_max_sf_payload(self, tx_dl: usize)` in `isotp.rs`**,
   delegating to the existing `max_sf_payload()` for `tx_dl <= 8` and
   computing `tx_dl - 2 - ae_len()` otherwise. This keeps the AE-byte
   arithmetic (`ae_len`, already private to `isotp.rs`) in one place and
   makes the two limits consistent by construction, rather than duplicating
   the formula in `rpc_primitive.rs`. `max_sf_payload()` itself is
   **unchanged** — it remains the sole limit the software-ISO-TP engine
   uses (that engine is Classic-CAN-only by its own module doc comment, and
   must never receive an FD-aware limit).

3. **A new shared `effective_fd_tx_dl(&ComParamSet) -> usize` helper**
   (`rpc_link.rs`) factors out the existing `.max(8)` unset-fallback rule
   `fd_can_tx_message_size_range` already implements, so both call sites
   share one definition of "the effective staged FD frame size" instead of
   two independently-written copies of the same fallback.

4. **`rpc_primitive.rs`'s functional-SF check** now selects
   `fd_max_sf_payload(effective_fd_tx_dl(...))` instead of
   `max_sf_payload()` when the link's `fd_base_family` (already computed a
   few lines above for the existing FD size-range split, ADR-159) is
   `Some(ISO15765)` — i.e. only for a native `FD_ISO15765_PS` link, never
   for Classic ISO15765, `FD_CAN_PS`, or a software-ISO-TP link (which is
   already rejected at connect time in combination with FD, per ADR-158's
   `fd_can_with_software_isotp_is_rejected`, making this case
   belt-and-braces rather than load-bearing there). The rejection message
   is extended to name the staged `CP_CANFDTxMaxDataLength` when the FD
   limit applied, so a client sees which ComParam to check.

Rejected alternatives (why not the other placements):

- **An optional FD parameter bolted onto `max_sf_payload()` itself** —
  every other link-state-dependent size limit in this codebase already
  keeps a separate function rather than parameterizing a pure/constant one
  (`fd_can_tx_message_size_range` alongside `tx_message_size_range`,
  `protocol.rs`'s segmented-message range functions) specifically so a
  Classic-only consumer can never accidentally receive or ignore the extra
  parameter. Adding one to `max_sf_payload()` risks exactly that for the
  software-ISO-TP engine.
- **A closed-form limit computed locally in `rpc_primitive.rs`** — would
  duplicate the AE-byte arithmetic outside `isotp.rs` for no compensating
  benefit.
- **Capping at a fixed 62 regardless of staged TX_DL** — would accept a
  payload the adapter's actual negotiated frame size can't carry when
  `CP_CANFDTxMaxDataLength` is staged below `64`.
- **Keying the cap off SAE J2534-2 Table 98's fixed `4128`-byte range** —
  wrong rule entirely: that constant bounds the overall segmented-message
  size (`protocol.rs::tx_message_size_range`'s FD arm), not a Single
  Frame's own capacity, which is governed by the live staged frame size.

## Consequences

- A functional `FD_ISO15765_PS` request now correctly accepts up to 62
  bytes (Normal) / 61 bytes (Extended) at `CP_CANFDTxMaxDataLength = 64`,
  scaling down per the table above for a smaller staged value; unset/`8`
  behavior is unchanged (still 7/6, matching Classic CAN and every prior
  release).
- ADR-055's Decision 3 conservatism (recorded in ADR-159's Consequences) is
  resolved for the `CoptSendrecv`/tester-present TX path this phase
  touches. See ADR-055/ADR-159's own Consequences sections for a pointer
  to this ADR.
- **Accepted residual, found while implementing this fix, deliberately not
  fixed here:** `rpc_misc.rs`'s `START_REPEAT_MESSAGE` handler (SAE
  J2534-2 clause 14, ADR-165/Phase 12) applies the SAE J2534-1/Table-98 TX
  size-range check but has **no ADR-055 functional-addressing Single
  Frame check at all**, for either Classic or FD framing — a
  functionally-addressed, oversized periodic ISO15765 message is never
  rejected there today. This predates this phase (ADR-055 itself never
  covered the repeat-message path, which didn't exist yet) and is a
  distinct code path from the one this ADR's Decision touches; tracked as
  its own follow-up in `j2534-0404-service/docs/implementation-notes.md`'s
  Prioritized Backlog rather than folded into this fix. **Now closed**
  (same PR sequence, `j2534-0404-service/docs/implementation-notes.md`'s
  formerly-open bullet for this gap): `ioctl_start_repeat_message` gained
  the identical functional-SF check this ADR's Decision item 4 added to
  `rpc_primitive.rs`, including the FD-aware widening.
- Tester-present's own `CP_TesterPresentMessage` length was, at the time of
  this ADR, unvalidated against any Single Frame limit on a native
  (non-software-ISO-TP) link — unaffected by and out of scope for this ADR.
  Closed by [ADR-215](ADR-215-tester-present-message-length-validation.md)
  Decision item 4, which reuses this ADR's own FD-aware
  `Addressing::max_sf_payload`/`fd_max_sf_payload` calculation for the
  tester-present case too.
