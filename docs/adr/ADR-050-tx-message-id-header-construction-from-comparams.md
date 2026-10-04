# ADR-050: Construct TX Message ID/Header from ComParams and UniqueRespIdTable

**Date:** 2026-07-03
**Status:** Accepted (KWP/ISO14230 header-composition shape superseded by ADR-166;
             scope narrowed to RawMode=OFF CLLs by ADR-196 (RawMode=ON skips this
             construction entirely); all other decisions below remain in force)
**Affects:** `j2534-0404-service/src/service/tx_header.rs` (new),
             `j2534-0404-service/src/service/rpc_primitive.rs`
             (`J2534Service::unique_resp_id_table_snapshot`, `rpc_start_com_primitive`),
             `j2534-0404-service/src/service/comparam_defaults.rs` (unchanged, reused),
             `j2534-0404-service/tests/grpc_mock.rs`

## Context

Before this change (ADR-049), `CoptSendrecv`'s `cop_data` was the literal
`PassThruMessage.Data` buffer: the client had to embed the CAN ID
(CAN/ISO15765), or a hand-built header (KWP/J1850), as the leading bytes of
`cop_data` itself. This duplicated information the service already has —
`CP_CanPhysReqId` and friends are configured per-ECU via
`SetUniqueRespIdTable` specifically so the service can route frames and
build filters (ADR-007, ADR-039/040/041); requiring the client to *also*
embed the same CAN ID in every `cop_data` was redundant and error-prone (a
client could send a CAN ID that didn't match its own configured addressing,
with no cross-check).

## Decision

`cop_data` (and `CP_TesterPresentMsg`) are now **payload-only**. A new
`tx_header::build_tx_message` constructs the full `PassThruMessage.Data`
buffer (ID/header prefix + payload) from the Active ComParam set and the
connecting CLL's `UniqueRespIdTable`, called from `rpc_start_com_primitive`
before the SAE J2534-1 size-range check (ADR-049) and before the primitive
is queued.

**Only the first `UniqueRespIdTable` entry is consulted**, regardless of how
many are configured. This was an explicit scope decision: the proto has no
per-`StartComPrimitive` field to designate which ECU an outgoing request
targets (`ExpectedResponseData.unique_resp_ids` only filters *incoming*
responses, ADR-014), and adding one would be a larger, separate interface
change. A CLL that needs to address more than one ECU with distinct
outgoing requests is out of scope for this decision; such a client still has
`CLEAR_MSG_FILTERS`/re-`SetUniqueRespIdTable` between requests as a
workaround, or can use one CLL per ECU (each shares the same physical
channel via `SharedChannel`, ADR-005 §"Physical Channel Sharing").

**Per protocol:**

- **CAN / ISO15765**: reuses the existing addressing resolution
  (`CP_CanPhysReqId`/`CP_CanPhysReqFormat`/`CP_CanPhysReqExtAddr`, previously
  matched by the CAN ID embedded in `cop_data` — now simply
  `entries.first()`, since there is no longer an embedded CAN ID to match
  against). A CAN/ISO15765 CLL with no `UniqueRespIdTable` entry (or one
  missing `CP_CanPhysReqId`) rejects `CoptSendrecv`/`CoptStartcomm` with a
  clear `Status::invalid_argument` — there is no COM-class fallback for a
  CAN ID (`CP_CanPhysReqId` is `PDU_PC_UNIQUE_ID` class, ADR-042, by design).
- **ISO9141 / ISO14230 (KWP)**: builds a 4-byte header — format byte
  (`CP_PhysReqFormatPriorityType`, default `0x80`: ISO14230-2's "physical
  addressing, length in a separate byte" encoding), target address (the
  UniqueRespIdTable entry's `CP_EcuRespSourceAddress` when present, else the
  COM-class `CP_PhysReqTargetAddr`, default `0x10` — both already have this
  exact default in `comparam_defaults.rs`), tester source address
  (`NODE_ADDRESS`, default `0xF1`), and an explicit length byte. No checksum
  byte is appended — this relies on the vendor DLL computing/verifying it
  (mirroring the existing `ISO9141_NO_CHECKSUM` connect-flag assumption:
  checksum is hardware-managed by default). This also fits the SAE J2534-1
  table's arithmetic exactly (4-byte header + 255-byte data = 259, the
  documented max) with no room left over for a manually-appended checksum.
- **J1850PWM / J1850VPW**: builds a 3-byte header the same way as KWP
  (target/source resolution identical), except the format/priority byte
  defaults to the standard OBD-II priority byte per protocol (`0x68` VPW /
  `0x61` PWM) rather than KWP's `0x80`, since J1850 has no equivalent to
  ISO14230's "length in format byte vs. separate byte" distinction. No CRC
  byte is appended (J1850's trailing CRC is vendor-DLL-managed, same
  rationale as KWP's checksum).
- **SCI**: unchanged — `cop_data` is forwarded as-is. SCI has no per-ECU
  addressing ComParam in this service's D-PDU mapping at all
  (`unique_id_params()` returns empty for SCI, `comparam_support.rs`), so
  there is nothing to construct a header from.

**Scope, matching explicit decisions made when this was scoped:**

- Functional/broadcast addressing (`CP_RequestAddrMode`'s functional
  branch, `CP_FuncReqTargetAddr`/`CP_FuncReqFormatPriorityType`) is not
  constructed. Only physical (one-to-one) addressing is built.
  **Superseded by ADR-054**, which builds functional addressing too.
- `CoptStartcomm`'s `init_data` (the ISO9141/ISO14230 wakeup frame) is
  unaffected — it remains raw, client-supplied bytes, consistent with
  ADR-049's scoping of that field. **Superseded by ADR-075**, which extends
  the payload-only + service-constructed-header contract to `init_data` too.

## Consequences

- **Software ISO-TP interaction (a bug caught during implementation):** the
  logical `[4-byte CAN ID][payload]` buffer `events.rs::isotp_send` slices
  with a fixed 4-byte offset must **never** carry an AE byte, regardless of
  addressing — the poll task's own frame builders
  (`isotp::single_frame`/`first_frame`/`consecutive_frame`) already prepend
  the AE to each individual CAN frame from `SoftIsoTpTx::tx_addressing`.
  `tx_header::build_tx_message` therefore takes a `software_isotp: bool`
  parameter that suppresses the AE byte in this one case; every other path
  (hardware ISO15765, raw CAN) includes it, since there the buffer *is* the
  literal `PassThruMessage.Data` sent to `PassThruWriteMsgs`. The SAE
  J2534-1 size-range check (ADR-049) mirrors this: in software-ISO-TP mode
  it always uses the Normal (non-extended) range against this buffer, since
  the buffer itself never grows by the AE byte there — the real per-frame
  capacity reduction under extended addressing is enforced separately by
  `isotp::Addressing`.
- A new `J2534Service::resolve_can_addressing`/`tx_header::resolve_can_addressing`
  (`entries.first()`-based) replaces the previous CAN-ID-matching
  `resolve_can_addressing` (which searched the table for the entry whose
  `CP_CanPhysReqId` equalled the CAN ID embedded in `cop_data`) — simpler,
  since there is no longer an embedded CAN ID to match against, and shared
  across `CoptSendrecv`'s software-ISO-TP branch and `CoptStartcomm`'s
  tester-present addressing (previously duplicated inline).
- `grpc_mock.rs`: every CAN-family test now calls a new `set_can_phys_req_id`
  helper (or an inline `SetUniqueRespIdTable` call) before sending, and
  `cop_data`/expected `written_data` assertions were split into
  payload-only-in / full-message-out pairs. KWP/J1850 tests rely on this
  service's documented ComParam-default fallback values rather than
  hand-crafted header bytes. Five new/updated boundary tests confirm the
  min/max payload lengths that produce a valid constructed message per
  protocol.
- `tests/live_grpc_flow.rs` required no changes: it already sent
  payload-only `cop_data` and called `SetUniqueRespIdTable` before
  `ConnectComLogicalLink` (a design choice made before ADR-049 temporarily
  required the client to embed the CAN ID) — this decision restores that
  test to being the exemplar for the client-facing contract, rather than an
  outlier.
- A deployment that previously embedded a CAN ID/header in `cop_data` (or in
  `CP_TesterPresentMsg`) must migrate to `SetUniqueRespIdTable` +
  payload-only `cop_data`. This is an intentional, documented breaking
  change to the client-facing message contract for `CoptSendrecv` and
  `CoptStartcomm`.
