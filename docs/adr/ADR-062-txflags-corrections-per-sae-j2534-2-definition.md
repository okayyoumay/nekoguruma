# ADR-062: TxFlags Corrections Per SAE J2534-2's Definition

**Date:** 2026-07-05
**Status:** Accepted (Decision item 2's `CAN_29BIT_ID`/`ISO15765_ADDR_TYPE` service-derived,
             client-input-discarded rule scoped to RawMode=OFF CLLs by ADR-196 — RawMode=ON
             makes those two bits client-authoritative instead; all other decisions below
             remain in force)
**Affects:** `j2534-0404-service/src/service/rpc_primitive.rs` (`compute_j2534_tx_flags`,
             `apply_resolved_tx_flags`, `rpc_start_com_primitive`),
             `j2534-0404-service/src/service/tx_header.rs` (`CanAddressing`,
             `can_addressing_tx_flags`, `resolve_can_addressing`),
             `j2534-0404-service/src/service.rs` (`ComParamSet::sci_tx_flags`),
             `j2534-0404-service/src/service/comparam_support.rs` (`is_sci_param`),
             `j2534-0404-service/src/service/events.rs` (`handle_start_comm`),
             `j2534-0404/src/lib.rs`

## Context

The caller supplied SAE J2534-2's `PASSTHRU_MSG.TxFlags` bit table and asked
that this service set it according to that definition. Auditing every TX
call site against the six named bits (`SCI_TX_VOLTAGE` bit 23, `SCI_MODE`
bit 22, `WAIT_P3_MIN_ONLY` bit 9, `CAN_29BIT_ID` bit 8,
`ISO15765_ADDR_TYPE` bit 7, `ISO15765_FRAME_PAD` bit 6) found four separate
gaps:

**1. `compute_j2534_tx_flags` silently dropped `TxFlagIso15765AddrType`.**
The proto `TxFlagBit` enum has a variant for it (`TX_FLAG_ISO15765_ADDR_TYPE
= 32`), but the function's `match` had no arm for it, so a client
explicitly requesting this bit via named `TxFlagBits` got `0` instead — an
outright bug, independent of anything else in this ADR.

**2. `CAN_29BIT_ID`/`ISO15765_ADDR_TYPE` were driven by client request, not
by the addressing this service had already resolved.** These two bits
describe objective facts about the message being built — whether its CAN ID
is 29-bit, whether an Address Extension byte was embedded — which this
service already computes from `CP_CanPhysReqFormat`/`CP_CanFuncReqFormat`
(Table B.13) for the message *body* (`tx_header::can_header_bytes`) and,
separately, for `FLOW_CONTROL_FILTER` construction
(`rpc_link::CanIdFormat::tx_flags`). The actual outgoing
`PassThruWriteMsgs`/`PassThruStartPeriodicMsg` calls never combined this
already-resolved information into `TxFlags` at all — the bits were 0 unless
a client happened to also request them manually via `TxFlagBits`, with no
requirement (or even a way, for the addressing itself) to keep the two
consistent. A client that requested `TxFlagCan29bitId` for an 11-bit
address, or omitted it for a 29-bit one, would produce a `TxFlags` value
inconsistent with the frame's actual body — and a J2534 adapter is meant to
interpret the CAN ID's width and the addressing from `TxFlags`, not by
inspecting the message content itself.

**3. `SCI_MODE`/`SCI_TX_VOLTAGE` had no path to ever be set.** No proto
`TxFlagBit` variant exists for either (only `WAIT_P3_MIN_ONLY`,
`CAN_29BIT_ID`, `ISO15765_ADDR_TYPE`, `ISO15765_FRAME_PAD` do), so a client
could only reach them via the raw-bytes escape hatch. Meanwhile
`CP_SCITransmitMode`/`CP_SCISetProgVoltage` — the D-PDU ComParams that
plainly exist to configure exactly this — were both documented in
`comparam-protocol-support.md` as "S: stored but not forwarded to
hardware," confirmed dead by grep (never read anywhere outside their
defaults). Additionally, `CP_SCISetProgVoltage` was not in `is_sci_param`'s
allowlist at all, meaning `SetComParam`/`GetComParam` rejected it outright —
it was not merely inert, it was unreachable.

**4. The periodic tester-present message ignored `TxFlags` entirely.**
`handle_start_comm`'s one-shot fast-init frame used the resolved `tx_flags`
parameter, but the recurring `PassThruStartPeriodicMsg` call a few lines
later hardcoded `j2534_0404::TX_NORMAL_TRANSMIT` (`0`) regardless — any bit
that should apply to every message from this CLL (addressing, SCI mode)
silently stopped applying after the first frame.

Two design decisions here needed the caller's input rather than inference,
since guessing wrong has real consequences:

- Whether `SCI_MODE`/`SCI_TX_VOLTAGE` should come from ComParams at all
  (versus adding new proto `TxFlagBit` variants and leaving them purely
  client-driven, like `WAIT_P3_MIN_ONLY`) — the caller confirmed ComParams.
- `CP_SCISetProgVoltage`'s value encoding is not documented anywhere in this
  repository, and the bit it drives is a physical action (applying 20V to
  the SCI bus after transmit) — not something to infer. The caller
  confirmed: any value other than the seeded `0xFFFF_FFFF` ("no override")
  default means apply the voltage, regardless of its specific numeric
  value.
- Whether `CAN_29BIT_ID`/`ISO15765_ADDR_TYPE` should be service-computed
  *authoritatively* (overriding client input) rather than merely
  supplementing it — the caller confirmed authoritative override, since
  they are objective facts already resolved elsewhere in this same request
  path, not caller preference.

## Decision

### `compute_j2534_tx_flags` no longer maps the two addressing bits at all

Rather than adding the missing `TxFlagIso15765AddrType` arm next to the
(now similarly pointless) `TxFlagCan29bitId` arm, both are removed from
this function entirely — every call site applies `apply_resolved_tx_flags`
immediately afterward, which unconditionally overwrites those two bit
positions, so mapping them here first would be dead work. The function's
doc comment states why.

### `tx_header::CanAddressing` gains `extended_can_id`

```rust
pub(super) struct CanAddressing {
    ...
    pub(super) extended_can_id: bool, // Table B.13 bit 1: 29-bit CAN ID
}
```

computed by a new `can_29bit_id(format_raw: Option<u32>) -> bool` helper
(`raw & 0x02 != 0`) alongside the existing `tx_addressing` resolution in
both branches of `resolve_can_addressing` — the same raw `CP_Can*Format`
value already being read, just also checked for this bit. This mirrors
`rpc_link::CanIdFormat::extended_can_id`'s identical decode for filter
construction; the two are not unified into one type in this change (they
serve different call sites with different surrounding structs), but the
bit-parsing formula is intentionally identical.

### `tx_header::can_addressing_tx_flags` and `rpc_primitive::apply_resolved_tx_flags`

```rust
pub(super) fn can_addressing_tx_flags(addressing: Option<CanAddressing>) -> u32 {
    // TX_EXTENDED_ID if addressing.extended_can_id, ISO15765_ADDR_TYPE if
    // addressing.tx_addressing is Extended(_); 0 for either when None.
}

fn apply_resolved_tx_flags(tx_flags: u32, can_addressing: Option<CanAddressing>, active: &ComParamSet) -> u32 {
    (tx_flags & !(TX_EXTENDED_ID | ISO15765_ADDR_TYPE))
        | can_addressing_tx_flags(can_addressing)
        | active.sci_tx_flags()
}
```

Applied in `rpc_start_com_primitive` for both `CoptSendrecv` and
`CoptStartcomm`, after the software-ISO-TP `TX_ISO15765_FRAME_PAD` masking
(unchanged) and before the value is stored into `TxItem`. `can_addressing`
is `None` for any non-CAN-family protocol, so both bits correctly clear to
`0` there — matching the spec's "reserved, shall be 0" for every protocol
but CAN/ISO15765.

### `ComParamSet::sci_tx_flags`

```rust
pub(super) fn sci_tx_flags(&self) -> u32 {
    // SCI_MODE if CP_SCITransmitMode != 0 (matches the TxFlags bit's own
    // 0=full-duplex/1=half-duplex meaning directly);
    // SCI_TX_VOLTAGE if CP_SCISetProgVoltage != 0xFFFF_FFFF (the seeded
    // "no override" default) -- any other value applies the voltage,
    // regardless of what specific value it is.
}
```

`0` for every non-SCI protocol, since both ComParams stay at their seeded
defaults (`0` and `0xFFFF_FFFF` respectively) there. `CP_SCISetProgVoltage`
is added to `is_sci_param`'s allowlist — it could not previously be set or
read at all. `j2534_0404::SCI_TX_VOLTAGE` is added to the `j2534-0404`
crate's re-export list (`TX_FLAG_SCI_TX_VOLTAGE` was already generated by
`bindgen` — every other `TX_FLAG_*` constant was already re-exported except
this one, a plain oversight in the hand-written list, not a bindgen
allowlist gap).

### Periodic tester-present uses the same resolved `TxFlags`

`handle_start_comm`'s `PassThruStartPeriodicMsg` call now passes the
function's `tx_flags` parameter (the same value the one-shot fast-init
frame already used) instead of the hardcoded `TX_NORMAL_TRANSMIT`.

## Consequences

- A client requesting `TxFlagCan29bitId`/`TxFlagIso15765AddrType` no longer
  has any effect on the outgoing `TxFlags` — these two positions are always
  computed from the CLL's resolved CAN addressing instead, even overriding
  an explicit (and possibly inconsistent) client request. Two existing
  tests (`comparam_tx.rs`) that asserted the old client-driven behavior for
  `TxFlagCan29bitId` were updated to instead configure
  `CP_CanPhysReqFormat`'s bit 1.
- `SCI_MODE`/`SCI_TX_VOLTAGE` now reflect `CP_SCITransmitMode`/
  `CP_SCISetProgVoltage` on every SCI-protocol transmission — a real
  behavior change for any caller that had been setting these ComParams
  expecting them to already do something (they had no effect before this
  ADR) or expecting them to remain inert (they now apply the 20V
  programming voltage per the value rule above).
- `tests/grpc_mock/comparam_tx.rs` gained four tests:
  `iso15765_addr_type_tx_flag_derived_from_extended_addressing_format`,
  `can_29bit_id_tx_flag_overrides_client_request_when_format_says_11bit`,
  `sci_tx_flags_derived_from_transmit_mode_and_prog_voltage_comparams`, and
  `sci_tx_voltage_flag_not_set_when_prog_voltage_left_at_default`. Verified
  against a temporarily neutralized `apply_resolved_tx_flags` (returning
  `tx_flags` unchanged) to confirm all four fail without the fix and pass
  with it restored.
- RC21/RC23 re-requests (`wait_for_expected_response`) resend the exact
  `tx_flags` value computed for the original send, so they inherit these
  corrections automatically — no separate change was needed there.
- The FlowControl-frame send in `process_frame_for_entry` (software ISO-TP)
  is unaffected: it already computes its own `TX_EXTENDED_ID` from the
  resolved `fc_dest` CAN ID's magnitude, independent of ComParam-based
  format resolution, and carries no addressing byte of its own to reflect
  via `ISO15765_ADDR_TYPE`. Left as-is; out of scope for this ADR.
