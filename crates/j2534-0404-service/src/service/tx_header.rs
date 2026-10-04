//! Constructs the protocol-specific CAN ID / header prefix that precedes
//! payload bytes in a `PassThruMessage.Data` buffer sent to the J2534
//! library, from ComParams (the Active set) and the connecting CLL's
//! UniqueRespIdTable. Only the first UniqueRespIdTable entry is consulted
//! when more than one is configured; outgoing requests always target that
//! entry's ECU (ADR-050).
//!
//! Two independent ComParams can select functional (broadcast) addressing,
//! chosen per call via [`AddrModeSource`]: `CP_RequestAddrMode` (1 =
//! physical, 2 = functional; ADR-054), used by `CoptSendrecv` and init TX
//! flags, and `CP_TesterPresentAddrMode` (0/absent = physical, 1 =
//! functional; ADR-138), used only by tester-present sends. Absent or any
//! other value defaults to physical for either ComParam, matching every
//! seeded default in `comparam_defaults.rs`. Functional (broadcast)
//! addressing is a COM-class, not per-ECU, ComParam (ADR-042) and is
//! therefore read directly from the Active set —
//! `CP_CanFuncReqId`/`Format`/`ExtAddr` for CAN/ISO15765,
//! `CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr` for KWP/J1850 — the
//! UniqueRespIdTable is not consulted in this branch (ADR-054).

use super::rpc_link::{usdt_resp_id, uudt_resp_id};
use super::*;

/// Selects which ComParam drives functional-vs-physical addressing for a
/// given call into this module (ADR-138). The two ComParams are
/// spec-independent of each other: a CLL can be configured functional for
/// one and physical for the other simultaneously.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AddrModeSource {
    /// `CP_RequestAddrMode` (ADR-054): functional iff `== Some(2)`.
    Request,
    /// `CP_TesterPresentAddrMode` (ADR-138): functional iff `== Some(1)`.
    TesterPresent,
}

/// CAN-family addressing resolved for the outgoing request: the CAN ID to
/// use (physical, from the CLL's first UniqueRespIdTable entry, or
/// functional, from the Active ComParam set's `CP_CanFuncReq*`), the
/// addressing to use when building outgoing frames on it, and (for software
/// ISO-TP under physical addressing) the paired FlowControl response CAN ID
/// and its own addressing.
#[derive(Debug, Clone, Copy)]
pub(super) struct CanAddressing {
    pub(super) req_id: u32,
    pub(super) tx_addressing: isotp::Addressing,
    pub(super) fc_can_id: Option<u32>,
    pub(super) fc_rx_addressing: isotp::Addressing,
    /// `true` when this addressing was resolved from `CP_RequestAddrMode`'s
    /// functional (broadcast) branch. ISO 15765-2 requires a functionally
    /// addressed request to fit in a single frame (ADR-055) — callers that
    /// need to enforce that check this instead of re-deriving it from
    /// `CP_RequestAddrMode` themselves.
    pub(super) functional: bool,
    /// `true` when the resolved `CP_Can*Format` (Table B.13 bit 1) marks
    /// `req_id` as a 29-bit CAN identifier. Drives `TX_FLAG_CAN_29BIT_ID` on
    /// the actual transmitted message — see `can_addressing_tx_flags`
    /// (ADR-062).
    pub(super) extended_can_id: bool,
}

/// Returns `true` when `source`'s ComParam selects functional (broadcast)
/// addressing. For `Request` (`CP_RequestAddrMode`), that's value `2`;
/// absent or any other value (including the standard physical value `1`)
/// defaults to physical. For `TesterPresent` (`CP_TesterPresentAddrMode`,
/// ADR-138), that's value `1`; absent or any other value defaults to
/// physical.
pub(super) fn use_functional_addressing(source: AddrModeSource, active: &ComParamSet) -> bool {
    match source {
        AddrModeSource::Request => active.unum32.get(&PARAM_REQUEST_ADDR_MODE).copied() == Some(2),
        AddrModeSource::TesterPresent => {
            active.unum32.get(&PARAM_TESTER_PRESENT_ADDR_MODE).copied() == Some(1)
        }
    }
}

/// Decodes a `CP_Can*Format` UNUM32 bitfield's Table B.13 bit 1 (CAN ID
/// Type): `true` for a 29-bit identifier, `false` for 11-bit or when the
/// ComParam is absent. Mirrors `rpc_link::CanIdFormat::extended_can_id`,
/// which decodes the same bit for `FLOW_CONTROL_FILTER` construction.
fn can_29bit_id(format_raw: Option<u32>) -> bool {
    format_raw.is_some_and(|raw| raw & 0x02 != 0)
}

/// The `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE` bits `addressing`
/// implies (`0` for both when `None`, e.g. a non-CAN-family protocol).
///
/// These describe objective facts about the message this service already
/// built from ComParams (the CAN ID's actual width, whether an AE byte was
/// embedded) — not caller preference — so callers apply this
/// authoritatively over whatever a client requested via `TxFlagBits` for
/// these two positions (ADR-062).
///
/// `hw_protocol_id` gates `ISO15765_ADDR_TYPE` (backlog fix, Codex review PR
/// #42 round 5 Finding K investigation): per SAE J2534-1 Table B.13, this bit
/// is an ISO15765-protocol extended-addressing indicator, so it must never be
/// set on a plain raw-CAN link — a `CanAddressing` with `tx_addressing`
/// resolved to `Extended` says only that the *ComParams* configure extended
/// addressing, not that the link's protocol is ISO15765 (a raw-CAN link's
/// physical/functional request ComParams share the same fields). Compared
/// via `resources::base_protocol_id` so an FD_ISO15765_PS/SW_ISO15765_PS/
/// `_PS`/`_CHx`-qualified ISO15765 link still counts (ADR-157/158/159/164),
/// mirroring `rpc_link::CanIdFormat::tx_flags`'s identical `filter_type` gate
/// for `FLOW_CONTROL_FILTER` construction. `TX_EXTENDED_ID` is unaffected —
/// it selects 11- vs. 29-bit CAN Id width and applies regardless of protocol.
pub(super) fn can_addressing_tx_flags(
    addressing: Option<CanAddressing>,
    hw_protocol_id: u32,
) -> u32 {
    let Some(a) = addressing else { return 0 };
    let mut flags = 0;
    if a.extended_can_id {
        flags |= j2534_0404::TX_EXTENDED_ID;
    }
    if matches!(a.tx_addressing, isotp::Addressing::Extended(_))
        && resources::base_protocol_id(hw_protocol_id) == j2534_0404::ISO15765
    {
        flags |= j2534_0404::ISO15765_ADDR_TYPE;
    }
    flags
}

/// Resolves CAN-family addressing per `addr_source`'s ComParam
/// (`CP_RequestAddrMode`, ADR-054, or `CP_TesterPresentAddrMode`, ADR-138).
///
/// Functional: reads the shared `CP_CanFuncReqId`/`Format`/`ExtAddr` from
/// `active`; `None` when `CP_CanFuncReqId` is not set. There is no
/// per-ECU FlowControl pairing for a broadcast request.
///
/// Physical (default): resolved from the first UniqueRespIdTable entry
/// (ADR-050: outgoing requests always target the first configured ECU).
/// `None` when the table is empty or its entry has no `CP_CanPhysReqId`.
pub(super) fn resolve_can_addressing(
    addr_source: AddrModeSource,
    active: &ComParamSet,
    entries: &[EcuUniqueRespEntry],
) -> Option<CanAddressing> {
    if use_functional_addressing(addr_source, active) {
        let req_id = active.unum32.get(&PARAM_CAN_FUNC_REQ_ID).copied()?;
        let format_raw = active.unum32.get(&PARAM_CAN_FUNC_REQ_FORMAT).copied();
        let tx_addressing = isotp::Addressing::from_format(
            format_raw,
            active
                .unum32
                .get(&PARAM_CAN_FUNC_REQ_EXT_ADDR)
                .copied()
                .unwrap_or(0) as u8,
        );
        return Some(CanAddressing {
            req_id,
            tx_addressing,
            fc_can_id: None,
            fc_rx_addressing: isotp::Addressing::Normal,
            functional: true,
            extended_can_id: can_29bit_id(format_raw),
        });
    }
    let entry = entries.first()?;
    let req_id = entry.params.unum32.get(&PARAM_CAN_PHYS_REQ_ID).copied()?;
    let format_raw = entry.params.unum32.get(&PARAM_CAN_PHYS_REQ_FORMAT).copied();
    let tx_addressing = isotp::Addressing::from_format(
        format_raw,
        entry
            .params
            .unum32
            .get(&PARAM_CAN_PHYS_REQ_EXT_ADDR)
            .copied()
            .unwrap_or(0) as u8,
    );
    // Backlog fix (edge-case-hunter, PR #42-adjacent UUDT/USDT sentinel
    // follow-up): a raw, unfiltered lookup here let a client-staged
    // ISO 22900-2 Table 76 `0xFFFFFFFF` "not used" sentinel for
    // `CP_CanRespUSDTId` reach `fc_can_id` as `Some(0xFFFFFFFF)` --
    // `SoftIsoTpTx.fc_can_id`'s own governing contract (below) treats `None`
    // as "accept any FlowControl frame" and `Some(id)` as "accept only
    // `id`," so the sentinel silently flipped that wildcard into "accept
    // none," since no real frame's CAN ID can ever equal `0xFFFFFFFF` --
    // every software-ISO-TP multi-frame TX on such a link would hang until
    // N_bs timeout instead of completing. Routed through `usdt_resp_id`,
    // mirroring every other USDT-sentinel fix site.
    let fc_can_id = usdt_resp_id(entry);
    let fc_rx_addressing = isotp::Addressing::from_format(
        entry
            .params
            .unum32
            .get(&PARAM_CAN_RESP_USDT_FORMAT)
            .copied(),
        entry
            .params
            .unum32
            .get(&PARAM_CAN_RESP_USDT_EXT_ADDR)
            .copied()
            .unwrap_or(0) as u8,
    );
    Some(CanAddressing {
        req_id,
        tx_addressing,
        fc_can_id,
        fc_rx_addressing,
        functional: false,
        extended_can_id: can_29bit_id(format_raw),
    })
}

/// Builds the CAN ID header bytes for `addressing`.
///
/// `include_ae` appends the Address Extension byte for extended addressing;
/// it must be `false` in software-ISO-TP mode, where the poll task's own
/// frame builders (`isotp::single_frame`/`first_frame`/`consecutive_frame`)
/// already prepend the AE to each individual CAN frame from
/// `SoftIsoTpTx::tx_addressing` — this outer buffer is the *logical*
/// multi-frame payload the poll task segments, always `[4-byte CAN ID][payload]`
/// regardless of addressing (`events.rs::isotp_send` slices a fixed 4-byte
/// offset). It must be `true` for every other path, where this buffer is the
/// literal `PassThruMessage.Data` sent to `PassThruWriteMsgs`.
fn can_header_bytes(addressing: &CanAddressing, include_ae: bool) -> Vec<u8> {
    let mut header = addressing.req_id.to_be_bytes().to_vec();
    if include_ae && let isotp::Addressing::Extended(ae) = addressing.tx_addressing {
        header.push(ae);
    }
    header
}

/// Builds a KWP2000 (ISO9141/ISO14230) addressing header. The wire shape is
/// gated by the format byte's top two bits (`format & 0xC0`), not bit 7
/// alone (ADR-166, ISO 22900-2 Table 76):
///
/// - `0x40` (CARB/ISO9141-2 exception addressing, e.g. this codebase's
///   `iso_15031_5_on_iso_9141_2`/`sae_j2190_on_iso_9141_2` presets'
///   `0x6C`/`0x68`, `comparam_defaults.rs`): always a 3-byte header
///   (format, target, source) -- there is never a separate trailing length
///   byte in this mode, regardless of payload length.
/// - `0x80`/`0xC0` (ISO14230 physical/functional addressing): format,
///   target, source, then either an explicit 4th length byte (when the
///   format's own low 6 bits are configured `0`, OR when the actual payload
///   is empty -- `0` recomposed into the low 6 bits would be
///   indistinguishable on the wire from "no length embedded, a separate
///   byte follows", so an empty payload always takes this shape too,
///   regardless of how the low 6 bits are configured) or the payload length
///   recomposed into those low 6 bits of the format byte itself (when
///   configured nonzero AND the payload is non-empty) -- capped at 63
///   (`0x3F`), returning `Err` above that since it cannot be encoded.
/// - `0x00` (unaddressed): always a 2-byte header (format, length byte),
///   with no target/source bytes, regardless of the low 6 bits.
///
/// No checksum byte is appended; this relies on the vendor DLL
/// computing/verifying it, the same as this service already assumes for
/// J1850 CRC.
///
/// Physical (default): format from `CP_PhysReqFormatPriorityType` (default
/// `0x80`), target from the first UniqueRespIdTable entry's
/// `CP_EcuRespSourceAddress` when present, else `CP_PhysReqTargetAddr`
/// (default `0x10`).
///
/// Functional (`addr_source`'s ComParam selects functional — either
/// `CP_RequestAddrMode` = 2, ADR-054, or `CP_TesterPresentAddrMode` = 1,
/// ADR-138): format from `CP_FuncReqFormatPriorityType` (default `0xC0`),
/// target from `CP_FuncReqTargetAddr` (default `0x33`) — a broadcast
/// request has no per-ECU target, so the UniqueRespIdTable is not
/// consulted.
fn kwp_header_bytes(
    addr_source: AddrModeSource,
    active: &ComParamSet,
    entries: &[EcuUniqueRespEntry],
    payload_len: usize,
) -> Result<Vec<u8>, String> {
    let functional = use_functional_addressing(addr_source, active);
    let format = if functional {
        active
            .unum32
            .get(&PARAM_FUNC_REQ_FORMAT_PRIORITY)
            .copied()
            .unwrap_or(0xC0) as u8
    } else {
        active
            .unum32
            .get(&PARAM_PHYS_REQ_FORMAT_PRIORITY)
            .copied()
            .unwrap_or(0x80) as u8
    };
    let target = if functional {
        active
            .unum32
            .get(&PARAM_FUNC_REQ_TARGET_ADDR)
            .copied()
            .unwrap_or(0x33) as u8
    } else {
        entries
            .first()
            .and_then(|e| e.params.unum32.get(&PARAM_ECU_RESP_SOURCE_ADDR).copied())
            .or_else(|| active.unum32.get(&PARAM_PHYS_REQ_TARGET_ADDR).copied())
            .unwrap_or(0x10) as u8
    };
    let source = active
        .unum32
        .get(&ComParamId(j2534_0404::NODE_ADDRESS))
        .copied()
        .unwrap_or(0xF1) as u8;
    match format & 0xC0 {
        0x40 => Ok(vec![format, target, source]),
        0x00 => Ok(vec![format, payload_len as u8]),
        _ => {
            if format & 0x3F == 0 || payload_len == 0 {
                // A zero-length payload can never be recomposed into the
                // embedded low-6-bits field: `0` there is indistinguishable
                // on the wire from "no length embedded -- a separate length
                // byte follows" (`kwp_header_and_payload_len`'s own parsing
                // rule, `events.rs`). Emitting the recompose branch's bare
                // 3-byte shape here would silently drop the length byte a
                // conforming receiver still expects. Fall back to the
                // explicit-length-byte shape instead (low 6 bits forced to
                // `0`, an explicit `0` length byte appended) rather than the
                // configured embedded-length encoding.
                Ok(vec![format & 0xC0, target, source, payload_len as u8])
            } else if payload_len > 0x3F {
                Err(format!(
                    "cop_data length {payload_len} exceeds the 63-byte maximum encodable in this \
                     KWP/ISO14230 format byte's embedded length field \
                     (CP_PhysReqFormatPriorityType/CP_FuncReqFormatPriorityType = {format:#04x})"
                ))
            } else {
                let recomposed_format = (format & 0xC0) | (payload_len as u8);
                Ok(vec![recomposed_format, target, source])
            }
        }
    }
}

/// Builds a J1850 (PWM/VPW) addressing header: header/priority byte, target
/// address, and tester source address (`NODE_ADDRESS`, default `0xF1`). No
/// CRC byte is appended; the vendor DLL computes/verifies J1850's trailing
/// CRC.
///
/// Physical (default): format from `CP_PhysReqFormatPriorityType`,
/// defaulting to the standard OBD-II priority byte for `native_protocol_id`
/// (`0x68` for J1850VPW, `0x61` for J1850PWM); target from the first
/// UniqueRespIdTable entry's `CP_EcuRespSourceAddress` when present, else
/// `CP_PhysReqTargetAddr` (default `0x10`).
///
/// Functional (`addr_source`'s ComParam selects functional — either
/// `CP_RequestAddrMode` = 2, ADR-054, or `CP_TesterPresentAddrMode` = 1,
/// ADR-138): format from `CP_FuncReqFormatPriorityType` (same
/// protocol-based default as physical), target from `CP_FuncReqTargetAddr`
/// (default `0x10`) — a broadcast request has no per-ECU target, so the
/// UniqueRespIdTable is not consulted.
fn j1850_header_bytes(
    native_protocol_id: u32,
    addr_source: AddrModeSource,
    active: &ComParamSet,
    entries: &[EcuUniqueRespEntry],
) -> Vec<u8> {
    let default_format = if native_protocol_id == j2534_0404::J1850VPW {
        0x68
    } else {
        0x61
    };
    let functional = use_functional_addressing(addr_source, active);
    let format = if functional {
        active
            .unum32
            .get(&PARAM_FUNC_REQ_FORMAT_PRIORITY)
            .copied()
            .unwrap_or(default_format) as u8
    } else {
        active
            .unum32
            .get(&PARAM_PHYS_REQ_FORMAT_PRIORITY)
            .copied()
            .unwrap_or(default_format) as u8
    };
    let target = if functional {
        active
            .unum32
            .get(&PARAM_FUNC_REQ_TARGET_ADDR)
            .copied()
            .unwrap_or(0x10) as u8
    } else {
        entries
            .first()
            .and_then(|e| e.params.unum32.get(&PARAM_ECU_RESP_SOURCE_ADDR).copied())
            .or_else(|| active.unum32.get(&PARAM_PHYS_REQ_TARGET_ADDR).copied())
            .unwrap_or(0x10) as u8
    };
    let source = active
        .unum32
        .get(&ComParamId(j2534_0404::NODE_ADDRESS))
        .copied()
        .unwrap_or(0xF1) as u8;
    vec![format, target, source]
}

/// Builds the full `PassThruMessage.Data` buffer (ID/header prefix +
/// `payload`) for `protocol`, from `active` ComParams and the connecting
/// CLL's UniqueRespIdTable (`entries`). Returns `Err` with a client-facing
/// message when the protocol requires addressing this service cannot
/// resolve (CAN family with no `CP_CanPhysReqId`, or under functional
/// addressing no `CP_CanFuncReqId`, configured). SCI and any unrecognised
/// protocol have no header concept here; `payload` is returned unchanged.
///
/// `software_isotp` must be `true` only for an ISO15765-family CLL running
/// in `can_channel_mode = "software-isotp"` — see `can_header_bytes` for why
/// this changes whether the AE byte is embedded here.
///
/// `addr_source` selects which ComParam decides functional-vs-physical for
/// this call (ADR-138): `AddrModeSource::Request` for `CoptSendrecv`/init TX
/// flags (`CP_RequestAddrMode`), `AddrModeSource::TesterPresent` for
/// tester-present sends (`CP_TesterPresentAddrMode`).
///
/// `tp20_established_tx_id` (ADR-188/Phase 7 Stage 7a, Codex review PR #97
/// fix): the TX-ID assigned by a TP2.0 connection this function's own
/// PROTOCOL_TP2_0_PS arm frames onto, or `None` when no TP2.0 connection is
/// currently `Established` for the calling CLL. This function stays
/// deliberately `LogicalLinkState`-blind otherwise (see the struct-level doc
/// comment above), but the established TX-ID must come from the real,
/// per-CLL connection phase tracked there (`LogicalLinkState::tp20_connection`)
/// -- NOT from a client-writable ComParam -- since `CP_TP20TxIdProposal` is
/// an ordinary Working ComParam a client can stage via `SetComParam` at any
/// time before `CoptStartcomm` ever runs (`comparam_support.rs`'s
/// `is_tp20_param` allowlists it), and `CoptSendrecv` has no `comm_started`
/// gate that would otherwise stop a spoofed value from reaching here (an
/// earlier version of this arm read `CP_TP20TxIdProposal` directly on the
/// false premise that `comm_started` could not become `true` without it
/// first being written back by `handle_start_comm` -- that write-back has
/// been removed; see `handle_start_comm`'s TP2.0 arm for why). Ignored by
/// every other protocol arm.
///
/// **Broadcast branch (ADR-192/Phase 7 Stage 7c):** when `active.
/// tp20_broadcast_address()` resolves `Some(address)` on a TP2.0 link, the
/// `PROTOCOL_TP2_0_PS` arm composes `[address] ++ payload` and returns
/// unconditionally, WITHOUT ever consulting `tp20_established_tx_id` --
/// unlike every other TP2.0 send, a broadcast has no Established-connection
/// precondition (clause 19.3.2.2 addresses a broadcast group, not an
/// established peer), so this branch succeeds even when no TP2.0 connection
/// has ever been started on the calling CLL. Only when the broadcast
/// ComParam is unset/`0` does the established-TX-ID logic below run,
/// unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_tx_message(
    protocol: ChannelProtocol,
    addr_source: AddrModeSource,
    active: &ComParamSet,
    entries: &[EcuUniqueRespEntry],
    payload: &[u8],
    software_isotp: bool,
    tp20_established_tx_id: Option<u32>,
    raw_mode: bool,
) -> Result<Vec<u8>, String> {
    // ADR-196 Decision item 2, extended by ADR-198 Phase 2: under RawMode,
    // `cop_data`/`CP_TesterPresentMsg` IS the literal `PassThruMessage.Data`
    // the client already assembled (CAN ID in Table D.4 bytes 0-3 for CAN/
    // ISO15765; the client's own KWP header -- and, iff ChecksumMode=OFF,
    // its own checksum byte -- for K-line; the client's own 3-byte J1850
    // header) -- header construction is entirely the client's own
    // responsibility, so this function returns the payload unchanged instead
    // of resolving/prepending a header. This check is protocol-agnostic and
    // unconditional: it runs before, and instead of, EVERY protocol match arm
    // below (CAN/ISO15765's `CP_CanPhysReqId`/`CP_CanFuncReqId` addressing
    // resolution, KWP's `kwp_header_bytes` composition), so extending the
    // RawMode-allowlisted protocol set at `CreateComLogicalLink` (`rpc_link::
    // rpc_create_com_logical_link`) needs no matching change here for most
    // protocols -- the allowlist alone decides which protocols ever reach
    // this function with `raw_mode = true`.
    //
    // **Correction, ADR-200 Phase 3:** that "no matching change here" claim
    // does NOT hold for SAE J1939. ISO 22900-2:2022 line 774/§10.1.4.19.5/
    // Table 80 define the D-PDU RawMode contract for J1939 as 4 bytes (the
    // 29-bit CAN ID only) plus payload -- the same shape as CAN/ISO15765 --
    // but SAE J2534-2 §16.4.3/Table 62 requires the NATIVE J2534 wire format
    // to carry a 5th byte, the destination address (DA), on every
    // `PassThruWriteMsgs` call on a J1939 channel (SAE J1939-21's
    // BAM-vs-RTS/CTS multi-packet transport selection depends on it). A
    // client's raw 4-byte-CAN-ID frame handed straight through would desync
    // every multi-byte payload by one byte and corrupt that transport
    // decision, so J1939 gets its own small shim (`raw_j1939_tx_message`)
    // instead of the generic passthrough -- see that function's own doc
    // comment.
    if raw_mode {
        return match protocol.j2534_protocol_id() {
            j2534_0404::PROTOCOL_J1939_PS => raw_j1939_tx_message(payload),
            _ => Ok(payload.to_vec()),
        };
    }
    match protocol.j2534_protocol_id() {
        j2534_0404::CAN | j2534_0404::ISO15765 => {
            let addressing =
                resolve_can_addressing(addr_source, active, entries).ok_or_else(|| {
                    if use_functional_addressing(addr_source, active) {
                        match addr_source {
                            AddrModeSource::Request => {
                                "sending functionally on this protocol requires \
                                 CP_CanFuncReqId to be set"
                                    .to_string()
                            }
                            AddrModeSource::TesterPresent => "functional addressing (selected by \
                                 CP_TesterPresentAddrMode = 1) requires CP_CanFuncReqId \
                                 to be set"
                                .to_string(),
                        }
                    } else {
                        "sending on this protocol requires a UniqueRespIdTable entry with \
                     CP_CanPhysReqId set (call SetUniqueRespIdTable before ConnectComLogicalLink \
                     or CoptSendrecv)"
                            .to_string()
                    }
                })?;
            let mut message = can_header_bytes(&addressing, !software_isotp);
            message.extend_from_slice(payload);
            Ok(message)
        }
        j2534_0404::ISO9141 | j2534_0404::ISO14230 => {
            // The KWP header's single length byte cannot encode more than 255
            // payload bytes. ISO14230's SAE J2534-1 frame ceiling (259) makes
            // an oversized payload unreachable there, but ISO9141's is 4128 --
            // without this check `payload_len as u8` below silently wraps and
            // a malformed frame reaches the wire.
            if payload.len() > u8::MAX as usize {
                return Err(format!(
                    "cop_data length {} exceeds the {}-byte maximum encodable in the \
                     single KWP length byte",
                    payload.len(),
                    u8::MAX
                ));
            }
            let mut message = kwp_header_bytes(addr_source, active, entries, payload.len())?;
            message.extend_from_slice(payload);
            Ok(message)
        }
        native_id @ (j2534_0404::J1850PWM | j2534_0404::J1850VPW) => {
            let mut message = j1850_header_bytes(native_id, addr_source, active, entries);
            message.extend_from_slice(payload);
            Ok(message)
        }
        j2534_0404::PROTOCOL_J1939_PS => {
            // ADR-179 Decision 3: the source address byte is this CLL's
            // currently claimed SAE J1939 address, written back into
            // `CP_TesterSourceAddress` (native `NODE_ADDRESS`) Active-set
            // by the claim loop (`events.rs::handle_start_comm`) on a
            // successful claim -- read the same way `tester_addr` reads it
            // for KWP/J1850 above. Before a claim ever completes this reads
            // the pre-claim default (`0xF1`, `tester_addr`'s own fallback);
            // in practice `CoptSendrecv`/the optional `CoptStartcomm`
            // message never reach this arm until `StartComm`'s claim loop
            // has already resolved, since J1939 CoptSendrecv requires
            // `comm_started` (ADR-179 does not relax that precondition).
            // `rpc_misc.rs::ioctl_start_repeat_message`'s own call to this
            // function is the one exception this codebase used to have
            // (SAE J2534-2 clause 14's `START` requires only a connected
            // CLL, not `comm_started`) -- closed by ADR-180 Decision 13
            // (round 11), which now rejects `START` on a negotiation-
            // enabled J1939 CLL with no claimed address yet, restoring
            // this comment's own "never reaches this arm unclaimed"
            // invariant for that path too (a non-negotiated CLL is
            // unaffected -- see Decision 13's own rationale for why).
            let mut message = j1939_header_bytes(tester_addr(active), active);
            message.extend_from_slice(payload);
            Ok(message)
        }
        j2534_0404::PROTOCOL_TP2_0_PS => {
            // SAE J2534-2 clause 19.3.2.2 (ADR-192/Phase 7 Stage 7c):
            // a broadcast send (`active.tp20_broadcast_address()` resolved
            // `Some`, gated by the caller the same way every other
            // ComParam-derived TxFlags bit in this service is gated --
            // `resources::is_tp2_0_protocol_id`, already implied here since
            // this arm only runs for a TP2.0 link) composes
            // `[address] ++ payload` directly, WITHOUT consulting
            // `tp20_established_tx_id` at all -- a broadcast is not
            // addressed to an established connection, so no
            // Established-phase precondition applies (ADR-192 Decision item
            // 1). Checked first, before the established-TX-ID branch below,
            // so a broadcast send succeeds even when no TP2.0 connection has
            // ever been started on this CLL.
            if let Some(address) = active.tp20_broadcast_address() {
                let mut message = vec![address];
                message.extend_from_slice(payload);
                return Ok(message);
            }
            // ADR-188/Phase 7 Stage 7a (Codex review PR #97 fix): prepends
            // the established 4-byte TX-ID from `tp20_established_tx_id`,
            // resolved by the caller from the CLL's real
            // `LogicalLinkState::tp20_connection` phase -- never from a
            // ComParam (see this function's own doc comment for why).
            // `None` (no TP2.0 connection currently `Established` for this
            // CLL) is the existing no-connection error mapping.
            let Some(tx_id) = tp20_established_tx_id else {
                return Err(
                    "sending on this TP2.0 ComLogicalLink requires an established connection \
                     (issue and complete a successful CoptStartcomm first)"
                        .to_string(),
                );
            };
            let mut message = tx_id.to_be_bytes().to_vec();
            message.extend_from_slice(payload);
            Ok(message)
        }
        _ => Ok(payload.to_vec()),
    }
}

/// SAE J2534-2 clause 14 Repeat Messaging (ADR-165 Decision 3): the header/ID
/// bytes identifying the addressed ECU's own *response*, prepended to a
/// client-supplied Repeat Messaging mask/pattern so a repeat slot's device-
/// side stop condition is scoped to that ECU rather than any frame matching
/// the client's payload-only bytes. This is the RX-direction counterpart of
/// [`build_tx_message`]'s TX-direction header -- always resolved from
/// `AddrModeSource::Request` (a repeat slot always targets one physical ECU,
/// mirroring `build_tx_message`'s own `CoptSendrecv` call sites; a
/// functionally-addressed slot has no single expected responder and is
/// rejected the same way [`resolve_can_addressing`] already rejects an
/// unresolvable functional request).
///
/// Returns `(header_bytes, tx_flags)`: `tx_flags` is the
/// `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE` bits implied by the RESPONSE-side
/// addressing `header_bytes` was built from -- callers composing a
/// `PassThruMessage` whose `Data` is (a client payload prefixed with)
/// `header_bytes` must use these flags for that message, not
/// [`can_addressing_tx_flags`]'s request-side flags (an earlier bug this
/// fixes: request and response can differ in CAN ID width or extended-
/// addressing mode, e.g. an 11-bit physical request paired with a 29-bit
/// UUDT/USDT response). Always `0` for a protocol with no CAN ID width/
/// extended-addressing concept (ISO9141/ISO14230/J1850/SCI).
///
/// CAN/ISO15765: [`resolve_can_addressing`] resolves the general
/// request-side addressing (used only to validate `CP_CanPhysReqId` is set
/// and to detect functional addressing, which clause 14 rejects -- see
/// below); the actual response id/format/ext-addr ComParams read depend on
/// `protocol` (an earlier bug this fixes -- reading only the ISO15765-
/// specific USDT fields regardless of protocol rejected plain CAN links
/// whose preset only configures UUDT):
/// - `ChannelProtocol::ISO15765`: `CP_CanRespUSDTId`/`_Format`/`_ExtAddr` --
///   ISO15765 point-to-point addressing is USDT by definition.
/// - `ChannelProtocol::CAN`: `CP_CanRespUUDTId`/`_Format`/`_ExtAddr` -- raw
///   CAN's own response addressing (mirrors `events_rx_routing.rs`'s
///   existing USDT/UUDT split read off the same UniqueRespIdTable entry).
///
/// `Err` when unresolvable (no `CP_CanPhysReqId`, functional addressing, or
/// a `CP_CanPhysReqId` entry with no paired response id ComParam for
/// `protocol`), mirroring `build_tx_message`'s own addressing errors.
///
/// ISO9141/ISO14230: mirrors the corresponding `kwp_header_bytes`
/// request-side builder's format/target/source triplet with target/source
/// swapped (the response's source is the ECU that owned the request's
/// target, and vice versa). The format byte comes from
/// `CP_PhysRespFormatPriorityType` (`PARAM_PHYS_RESP_FORMAT_PRIORITY`) -- the
/// response-side counterpart of `kwp_header_bytes`'/`j1850_header_bytes`'s
/// own `CP_PhysReqFormatPriorityType` request-side read; these genuinely
/// differ under real presets (e.g. `iso_15031_5_on_sae_j1850_vpw`'s distinct
/// `phys_format`/`phys_resp_format` defaults in `comparam_defaults.rs`), so
/// reusing the request-side ComParam here (an earlier bug this fixes) would
/// silently prepend the wrong format byte to the mask/pattern and never
/// match the real ECU response. A functionally-addressed CLL (`addr_source`
/// = `AddrModeSource::Request` selects functional, ADR-054) is rejected up
/// front, same as the CAN branch above and consistent with this function's
/// own doc comment: clause 14 has no broadcast/functional stop-condition
/// concept (ADR-165 Decision 3), so there is no functional-format branch to
/// resolve here at all.
///
/// ADR-166: like [`kwp_header_bytes`]'s request-side composition, the
/// response-side wire shape is gated by the format byte's top two bits
/// (`format & 0xC0`), not bit 7 alone (ISO 22900-2 Table 76) --
/// `0x40` (CARB/ISO9141-2 exception addressing) is always a 3-byte header
/// (format, tester, ECU) with no separate length byte at all; `0x80`/`0xC0`
/// (ISO14230 physical/functional addressing) carries the 2-byte
/// tester/ECU address pair, plus a separate trailing length byte whenever
/// the format's own low 6 bits (`format & 0x3F`, the embedded LEN field
/// `kwp_header_and_payload_len`'s own parsing reads) are configured `0`;
/// `0x00` (unaddressed) has no address bytes at all, plus an unconditional
/// trailing length byte (present regardless of the low 6 bits -- unlike the
/// `0x80`/`0xC0` case, unaddressed mode has no embedded-length variant).
/// Getting either gate wrong
/// shifts every mask/pattern byte's alignment relative to the real wire
/// frame (earlier bugs this fixes -- Codex review PR #42 rounds 2 and 16).
/// Where a separate length byte is present, this function appends it as a
/// WILDCARD position instead of a predicted value: its actual value is
/// payload-length-dependent, not part of the ECU's static address identity
/// a repeat slot's mask/pattern scopes against, and cannot be predicted
/// here. See the returned `header_mask`. The embedded-length case (low 6
/// bits nonzero) has the identical unpredictability, just packed into the
/// format byte itself rather than a separate wire byte -- this function
/// wildcards only those 6 bits there (mask `0xC0`, keeping the top-2-bit
/// address-mode discriminator an exact match), a partial-byte sibling of the
/// same WILDCARD treatment (backlog fix, round 20 sibling gap to Finding B).
/// J1850 shares no such concept
/// (`j1850_header_bytes`'s own fixed 3-byte, no-length-byte shape, confirmed
/// against `header_footer_len`'s identical J1850 framing in `events.rs`) and
/// so needs no equivalent handling.
///
/// `PROTOCOL_TP2_0_PS`: the incoming frame's leading 4 bytes are the RX-ID
/// header (clause 19.4.4 Table 81), exact-matched against
/// `tp20_established_rx_id` -- see this function's own TP2.0 match arm for
/// why that value must come from the CLL's real `LogicalLinkState::
/// tp20_connection` phase, mirroring [`build_tx_message`]'s identical
/// `tp20_established_tx_id` parameter (Codex review PR #97, ADR-188 Fix H).
/// `Err` when no TP2.0 connection is currently `Established` for the CLL.
///
/// Any other protocol: `Ok((Vec::new(), Vec::new(), 0))` (no header
/// concept), mirroring `build_tx_message`'s own fallback -- the client's
/// mask/pattern is used unprefixed.
///
/// Returns `(header_bytes, header_mask, tx_flags)`: `header_mask` is the
/// same length as `header_bytes`, all `0xFF` (exact match required) except
/// at a KWP/ISO14230/ISO9141 separate-length-byte position, which is `0x00`
/// (don't-care -- this codebase's existing zero-mask convention, ADR-005/
/// ADR-122's pass-all-filter precedent: `(data[i] & 0x00) == pattern[i]` is
/// only trivially satisfiable when `pattern[i] == 0`, so `header_bytes`
/// itself carries a `0` placeholder at that position too). Callers must
/// prepend both `header_bytes` and `header_mask` to the client's own
/// pattern/mask (not just `header_bytes` to an assumed-all-ones prefix, an
/// earlier caller-side bug this return-shape change fixes structurally).
pub(super) fn response_header_bytes(
    protocol: ChannelProtocol,
    active: &ComParamSet,
    entries: &[EcuUniqueRespEntry],
    tp20_established_rx_id: Option<u32>,
) -> Result<(Vec<u8>, Vec<u8>, u32), String> {
    match protocol.j2534_protocol_id() {
        j2534_0404::CAN | j2534_0404::ISO15765 => {
            let addressing = resolve_can_addressing(AddrModeSource::Request, active, entries)
                .ok_or_else(|| {
                    "SAE J2534-2 clause 14 Repeat Messaging's stop-condition mask/pattern \
                     requires a UniqueRespIdTable entry with CP_CanPhysReqId set (call \
                     SetUniqueRespIdTable before ConnectComLogicalLink)"
                        .to_string()
                })?;
            let is_iso15765 = protocol.j2534_protocol_id() == j2534_0404::ISO15765;
            let (format_param, ext_addr_param, param_name) = if is_iso15765 {
                (
                    PARAM_CAN_RESP_USDT_FORMAT,
                    PARAM_CAN_RESP_USDT_EXT_ADDR,
                    "CP_CanRespUSDTId",
                )
            } else {
                (
                    PARAM_CAN_RESP_UUDT_FORMAT,
                    PARAM_CAN_RESP_UUDT_EXT_ADDR,
                    "CP_CanRespUUDTId",
                )
            };
            // Functional addressing has no single expected responder --
            // reject the same way the request-side already does, rather
            // than consult a UniqueRespIdTable entry that may happen to be
            // configured anyway (ADR-054's request-side functional branch
            // never consults the table either).
            let entry = if addressing.functional {
                None
            } else {
                entries.first()
            };
            // ISO 22900-2 Table 76 documents `0xFFFFFFFF` as the "not used"
            // sentinel for BOTH `CP_CanRespUUDTId` and, identically, two
            // entries later in the same table, `CP_CanRespUSDTId` -- it can
            // be present in an unedited/default UniqueRespIdTable entry
            // (confirmed by several raw-CAN presets in comparam_defaults.rs
            // that seed the UUDT ComParam to exactly this value) even though
            // the field was never actually configured -- treating any
            // present value as a real address would build this
            // stop-condition mask/pattern template around an invalid CAN
            // identifier instead of rejecting the request the same way an
            // entirely-absent ComParam already is below. Reuse
            // `rpc_link::uudt_resp_id`/`rpc_link::usdt_resp_id`'s existing
            // filters rather than re-deriving the sentinel check (Codex
            // review, ADR-165 PR #42 round 19; corrected round 19 follow-up
            // after direct spec verification showed USDT shares the
            // identical sentinel). Every ISO15765 preset in
            // comparam_defaults.rs happens to seed `CP_CanRespUSDTId` with a
            // real response ID, never `0xFFFFFFFF` -- but that is a
            // default-authoring convention only, not something
            // `SetUniqueRespIdTable`'s own validation (rpc_misc.rs) enforces,
            // so a client can still legitimately stage the sentinel there and
            // both branches must filter it identically.
            let resp_id = if is_iso15765 {
                entry.and_then(usdt_resp_id)
            } else {
                entry.and_then(uudt_resp_id)
            }
            .ok_or_else(|| {
                format!(
                    "SAE J2534-2 clause 14 Repeat Messaging's stop-condition mask/pattern \
                     requires the UniqueRespIdTable entry's {param_name} (this CLL's \
                     expected physical response address) to be set"
                )
            })?;
            let format_raw = entry.and_then(|e| e.params.unum32.get(&format_param).copied());
            let ext_addr = entry
                .and_then(|e| e.params.unum32.get(&ext_addr_param).copied())
                .unwrap_or(0) as u8;
            let rx_addressing = isotp::Addressing::from_format(format_raw, ext_addr);
            let mut header = resp_id.to_be_bytes().to_vec();
            if let isotp::Addressing::Extended(ae) = rx_addressing {
                header.push(ae);
            }
            let mut tx_flags = 0u32;
            if can_29bit_id(format_raw) {
                tx_flags |= j2534_0404::TX_EXTENDED_ID;
            }
            // Finding D (Codex review, ADR-165 PR #42 round 3):
            // ISO15765_ADDR_TYPE is an ISO15765-only extended-addressing
            // indicator per SAE J2534-1 Table B.13 (see rpc_link.rs's
            // `CanIdFormat::tx_flags`, Codex review PR #32, for the
            // established precedent) -- a conforming adapter may reject a
            // raw-CAN PASS_FILTER/BLOCK_FILTER-equivalent message that sets
            // it. The `ae` byte is still appended to the header above for
            // both protocols (it identifies the address on the wire), but
            // only ISO15765 tells the device to interpret it via this flag.
            if is_iso15765 && matches!(rx_addressing, isotp::Addressing::Extended(_)) {
                tx_flags |= j2534_0404::ISO15765_ADDR_TYPE;
            }
            let mask = vec![0xFFu8; header.len()];
            Ok((header, mask, tx_flags))
        }
        j2534_0404::ISO9141 | j2534_0404::ISO14230 => {
            if use_functional_addressing(AddrModeSource::Request, active) {
                return Err(
                    "SAE J2534-2 clause 14 Repeat Messaging has no functional (broadcast) \
                     stop-condition concept -- a repeat slot always targets one physical ECU; \
                     this CLL is functionally addressed (CP_RequestAddrMode = 2)"
                        .to_string(),
                );
            }
            // Codex review, ADR-165 PR #42 round 11: `CP_PhysRespFormatPriorityType`
            // is a `PDU_PC_UNIQUE_ID`-classified ComParam per the SAE J2534-1
            // ComParam table -- a client configures it per-ECU, in the
            // selected UniqueRespIdTable entry, not in the common Active
            // ComParamSet. Resolved with the same entries-first-then-active
            // fallback chain `ecu_addr` (below) already establishes for the
            // other `PDU_PC_UNIQUE_ID`-scoped param this function reads two
            // lines down -- reading `format` from `active` alone (as before)
            // missed a per-ECU override and could compose the repeat-message
            // stop mask/pattern (and the length-byte gate just below) against
            // the wrong response format entirely.
            let format = entries
                .first()
                .and_then(|e| {
                    e.params
                        .unum32
                        .get(&PARAM_PHYS_RESP_FORMAT_PRIORITY)
                        .copied()
                })
                .or_else(|| active.unum32.get(&PARAM_PHYS_RESP_FORMAT_PRIORITY).copied())
                .unwrap_or(0x80) as u8;
            // ADR-166: the top two bits of the format byte jointly select
            // the wire shape (ISO 22900-2 Table 76) -- see this function's
            // own doc comment above.
            let mut header = vec![format];
            let mut mask = vec![0xFFu8];
            match format & 0xC0 {
                0x40 => {
                    // CARB/ISO9141-2 exception addressing (ADR-166): always
                    // addressed, never has a separate trailing length byte.
                    header.push(tester_addr(active));
                    header.push(ecu_addr(entries, active));
                    mask.push(0xFF);
                    mask.push(0xFF);
                }
                0x00 => {
                    // Unaddressed (ADR-166): format + trailing length byte,
                    // always -- no target/source bytes. Wildcarded: the real
                    // value is payload-length-dependent and unpredictable
                    // here.
                    header.push(0);
                    mask.push(0x00);
                }
                _ => {
                    // 0x80/0xC0: ISO14230 physical/functional addressing
                    // (unchanged from the pre-ADR-166 behavior).
                    header.push(tester_addr(active));
                    header.push(ecu_addr(entries, active));
                    mask.push(0xFF);
                    mask.push(0xFF);
                    if format & 0x3F == 0 {
                        // Finding B (round 2): a separate trailing length
                        // byte follows whenever the low 6 bits are
                        // configured as 0; wildcarded, since its real value
                        // is payload-length-dependent.
                        header.push(0);
                        mask.push(0x00);
                    } else {
                        // Backlog fix (round 20 sibling gap to Finding B):
                        // the embedded-length case has no separate length
                        // byte, but the format byte's own low 6 bits ARE
                        // that length -- payload-length-dependent and
                        // equally unpredictable from ComParams alone. Unlike
                        // Finding B's whole-byte wildcard, only the low 6
                        // bits are wildcarded here (mask 0xC0): the top 2
                        // bits are this branch's own `format & 0xC0`
                        // address-mode discriminator (0x80 physical / 0xC0
                        // functional) and must stay an exact match. Per this
                        // codebase's zero-mask convention (`data[i] &
                        // mask[i] == pattern[i]`), header[0]'s own low 6
                        // bits are zeroed to match -- the real ECU
                        // response's format byte can differ there without
                        // breaking the match.
                        header[0] &= 0xC0;
                        mask[0] = 0xC0;
                    }
                }
            }
            Ok((header, mask, 0))
        }
        native_id @ (j2534_0404::J1850PWM | j2534_0404::J1850VPW) => {
            if use_functional_addressing(AddrModeSource::Request, active) {
                return Err(
                    "SAE J2534-2 clause 14 Repeat Messaging has no functional (broadcast) \
                     stop-condition concept -- a repeat slot always targets one physical ECU; \
                     this CLL is functionally addressed (CP_RequestAddrMode = 2)"
                        .to_string(),
                );
            }
            let default_format = if native_id == j2534_0404::J1850VPW {
                0x68
            } else {
                0x61
            };
            // Codex review, ADR-165 PR #42 round 12: closes the sibling gap
            // round 11's own fix (a few lines up, for the ISO9141/ISO14230
            // branch) deliberately left open pending confirmation --
            // `CP_PhysRespFormatPriorityType` is classified `PDU_PC_UNIQUE_ID`
            // per the SAE J2534-1 ComParam table regardless of the underlying
            // protocol family, so J1850 needs the identical entries-first-
            // then-active fallback chain. Only the resolution source changes
            // here; J1850's own `default_format` (computed above from
            // `native_id`) remains the correct final fallback, not KWP's
            // hardcoded `0x80`.
            let format = entries
                .first()
                .and_then(|e| {
                    e.params
                        .unum32
                        .get(&PARAM_PHYS_RESP_FORMAT_PRIORITY)
                        .copied()
                })
                .or_else(|| active.unum32.get(&PARAM_PHYS_RESP_FORMAT_PRIORITY).copied())
                .unwrap_or(default_format) as u8;
            // J1850 has no separate-length-byte concept -- always the fixed
            // 3-byte format/target/source header, mirroring
            // `j1850_header_bytes`'s own request-side shape (confirmed
            // against `header_footer_len`'s identical J1850 framing,
            // `events.rs`).
            let header = vec![format, tester_addr(active), ecu_addr(entries, active)];
            let mask = vec![0xFFu8; header.len()];
            Ok((header, mask, 0))
        }
        j2534_0404::PROTOCOL_J1939_PS => {
            // ADR-179 Decision 9 (design-advisor consult, Codex review PR
            // #72 round 9): unlike CAN/KWP/J1850 above, only ONE of the
            // response's five header bytes is knowable from ComParams at
            // repeat-slot setup time. Bytes 0-2 (priority/EDP/DP and the
            // response PGN) depend on the ANSWERING message's own PGN,
            // which is not necessarily this CLL's own outbound
            // `CP_J1939PDUFormat`/`CP_J1939PDUSpecific` (`j1939_header_bytes`'s
            // own REQUEST-direction fields) -- and no ComParam records an
            // "expected response PGN" at all. (This citation used to read
            // "`comparam_support::unique_id_params` has no J1939 branch" --
            // ADR-184 gave that function a J1939 branch, but only for
            // `CP_J1939SourceAddress`; no ComParam of any UNIQUE_ID class
            // records an expected PGN, so this specific gap is unchanged.)
            // Byte 4 (destination) is unknowable for the same reason PLUS a
            // second, independent one: SAE J1939-21 PDU2 (broadcast/BAM)
            // responses -- the common case, since most requested PGNs are
            // PDU2 -- carry the global address there regardless of who
            // asked, so exact-matching this CLL's own claimed address would
            // make a repeat slot never see a broadcast answer. All four
            // wildcard, per this function's own "wildcard what's
            // unknowable" convention.
            //
            // Byte 3 (the responding ECU's SAE J1939 source address) is the
            // one exception: the swapped-direction counterpart of
            // `CP_J1939TargetAddress` (this CLL's own configured
            // destination), mirroring `tester_addr`/`ecu_addr`'s own
            // swapped-direction reuse above. Rejected (mirroring the
            // KWP/J1850/CAN functional-addressing `Err`s: "a repeat slot
            // always targets one physical ECU") when the target is
            // unconfigured (the `0xFFFF` "not configured" sentinel,
            // ADR-179 Decision 4) or `0xFF` (BAM/global -- no single
            // expected responder, clause 16.4.4).
            //
            // ADR-184 residual (design-advisor flagged genuine uncertainty
            // here, evaluated and deliberately left as-is, not wired): now
            // that `entries` CAN carry a per-ECU `CP_J1939SourceAddress`
            // (`UniqueRespIdKey`), KWP/J1850 above mirror an
            // entries-first-then-active fallback for byte 3 too. That
            // pattern resolves the SAME ComParam from two sources
            // (`entries.first()` overriding `active`'s coarser value); it
            // does not transfer cleanly here because entries' own
            // `CP_J1939SourceAddress` and `active`'s `CP_J1939TargetAddress`
            // are DIFFERENT ComParams with DIFFERENT sentinel semantics --
            // `CP_J1939TargetAddress`'s `0xFFFF`/`0xFF` sentinels (checked
            // below) mean "not configured"/"BAM broadcast" specifically for
            // a TARGET address; `0` and even `0xFFFF` are themselves
            // ordinary values in `CP_J1939SourceAddress`'s own value space
            // (an SA of 0 is a real ECU address; the `>0xFF` extended range
            // is this struct's own accepted "unmatchable in practice"
            // residual, not "unconfigured"). Reusing the `target_address`
            // match arms below against an entries-sourced SA would silently
            // misapply TARGET-address-specific sentinel meaning to a
            // SOURCE-address value space that does not share it -- a
            // correctness risk, not a mechanical mirror -- so byte 3 stays
            // resolved from `CP_J1939TargetAddress` only, unchanged from
            // pre-ADR-184. Wiring entries-first SA resolution here (if ever
            // wanted) needs its own sentinel/precedence design, not a copy
            // of this function's KWP/J1850 pattern.
            //
            // `TX_EXTENDED_ID` is set unconditionally: J1939 frames are
            // always 29-bit (clause 16.4.3), and the mock's own repeat-slot
            // matcher requires an honestly-flagged RX's `CAN_29BIT_ID` bit
            // to agree with the template's TxFlags.
            let target_address = active.unum32.get(&PARAM_J1939_TARGET_ADDRESS).copied();
            let responder = match target_address {
                None | Some(0xFFFF) => {
                    return Err(
                        "SAE J2534-2 clause 14 Repeat Messaging's stop-condition mask/pattern \
                         requires CP_J1939TargetAddress to be configured (not the 0xFFFF \"not \
                         configured\" sentinel)"
                            .to_string(),
                    );
                }
                Some(0xFF) => {
                    return Err(
                        "SAE J2534-2 clause 14 Repeat Messaging has no functional (broadcast) \
                         stop-condition concept -- a repeat slot always targets one physical \
                         ECU; this CLL's CP_J1939TargetAddress selects BAM (broadcast, 0xFF)"
                            .to_string(),
                    );
                }
                // Codex review finding (PR #72 round 11): `PDU_IOCTL_START_
                // REPEAT_MESSAGE` requires only a connected CLL, not
                // `comm_started` -- so this arm, unlike `build_tx_message`'s
                // own J1939 arm, is reachable before `StartComPrimitive`'s
                // one-byte-range gate (`rpc_primitive.rs`, mirrored here)
                // ever runs. Without this check, an out-of-range
                // `CP_J1939TargetAddress` (e.g. `0x100`) fell through to the
                // `as u8` cast below, silently truncating to a different
                // responder than `GetComParam` still reports. Reject here
                // too, mirroring `StartComPrimitive`'s own check.
                Some(addr) if addr > 0xFF => {
                    return Err(format!(
                        "CP_J1939TargetAddress ({addr:#06x}) must fit in one byte (0x00-0xFF, \
                         or the 0xFFFF \"not configured\" sentinel) before a SAE J2534-2 clause \
                         14 Repeat Messaging stop-condition mask/pattern can be composed"
                    ));
                }
                Some(addr) => addr as u8,
            };
            let header = vec![0, 0, 0, responder, 0];
            let mask = vec![0x00, 0x00, 0x00, 0xFF, 0x00];
            Ok((header, mask, j2534_0404::TX_EXTENDED_ID))
        }
        j2534_0404::PROTOCOL_TP2_0_PS => {
            // Codex review fix (PR #97, ADR-188, Fix H): mirrors
            // `build_tx_message`'s own PROTOCOL_TP2_0_PS arm -- the incoming
            // frame's first 4 bytes are the TP2.0 RX-ID header (clause 19.4.4
            // Table 81), not payload, so a repeat slot's client-supplied
            // stop-condition mask/pattern (which describes payload content)
            // must be prefixed with an exact-match template for those 4
            // bytes, the same way CAN/ISO15765 above prefix the expected
            // response CAN ID. `tp20_established_rx_id` is resolved by the
            // caller from this CLL's real `LogicalLinkState::tp20_connection`
            // phase (its `requested_rx_id`) -- never a ComParam, for the
            // identical reason `tp20_established_tx_id` is (see this
            // function's and `build_tx_message`'s shared doc comment). `None`
            // (no TP2.0 connection currently `Established`) is rejected, per
            // ADR-173 Decision 4's "reject a condition whose stop criterion
            // could never evaluate correctly" precedent -- without a real
            // RX-ID there is no correct 4-byte template to compose here.
            let Some(rx_id) = tp20_established_rx_id else {
                return Err(
                    "SAE J2534-2 clause 14 Repeat Messaging's stop-condition mask/pattern on \
                     this TP2.0 ComLogicalLink requires an established connection (issue and \
                     complete a successful CoptStartcomm first)"
                        .to_string(),
                );
            };
            let header = rx_id.to_be_bytes().to_vec();
            let mask = vec![0xFFu8; header.len()];
            // Codex review finding (P2, PR #97, round 18): unlike CAN/
            // ISO15765 above (which derive TX_EXTENDED_ID from the
            // response addressing's own configured format), this arm
            // returned `0` unconditionally -- when the established RX-ID
            // requires a 29-bit identifier (above 0x7FF), the repeat
            // slot's own device-side stop-condition template was
            // evaluated as an 11-bit template instead, and the device may
            // never recognize a match on the intended extended-ID
            // response. Mirrors `apply_resolved_tx_flags`'s identical
            // TP2.0 fix (this function's own sibling `build_tx_message`'s
            // TX-side counterpart).
            let tx_flags = if rx_id > 0x7FF {
                j2534_0404::TX_EXTENDED_ID
            } else {
                0
            };
            Ok((header, mask, tx_flags))
        }
        _ => Ok((Vec::new(), Vec::new(), 0)),
    }
}

/// `CP_NODE_ADDRESS`'s tester source address (default `0xF1`) -- shared by
/// [`kwp_header_bytes`]/[`j1850_header_bytes`]'s own identical read and
/// [`response_header_bytes`]'s swapped-direction reuse of the same value.
/// `pub(super)` (not module-private): `rpc_primitive.rs::resolve_send_recv_tx`
/// (ADR-180 Decision 14, PR #72 round 12) also reads this SAME resolved
/// J1939 source-address byte to populate `ResolvedSendRecvTx::j1939_tx_source`
/// for `events.rs::handle_send_recv`'s transmit-time claim-drift check --
/// it must read the identical value [`build_tx_message`]'s
/// `PROTOCOL_J1939_PS` arm composed the frame's header with, not
/// re-derive it independently.
pub(super) fn tester_addr(active: &ComParamSet) -> u8 {
    active
        .unum32
        .get(&ComParamId(j2534_0404::NODE_ADDRESS))
        .copied()
        .unwrap_or(0xF1) as u8
}

/// The physically-addressed ECU's own address -- the first UniqueRespIdTable
/// entry's `CP_EcuRespSourceAddress` when present, else `CP_PhysReqTargetAddr`
/// (default `0x10`). Shared by [`kwp_header_bytes`]/[`j1850_header_bytes`]'s
/// own identical `target` read (request direction) and
/// [`response_header_bytes`]'s swapped-direction reuse as the response's
/// `source` (this ECU is who sends the response).
fn ecu_addr(entries: &[EcuUniqueRespEntry], active: &ComParamSet) -> u8 {
    entries
        .first()
        .and_then(|e| e.params.unum32.get(&PARAM_ECU_RESP_SOURCE_ADDR).copied())
        .or_else(|| active.unum32.get(&PARAM_PHYS_REQ_TARGET_ADDR).copied())
        .unwrap_or(0x10) as u8
}

/// Builds the SAE J2534-2 clause 16.4.3/16.4.4 SAE J1939 5-byte message
/// prefix (ADR-179 Decision 6): bytes 0-3 are the 29-bit CAN identifier
/// SAE J1939-21 defines (`Data[0]`'s top 3 bits always zero, per clause
/// 16.4.3), byte 4 is the message-level destination address governing
/// clause 16.4.4's BAM-vs-Connection-Management segmentation choice.
///
/// The identifier assembles SAE J1939-21's standard PDU1/PDU2 layout from
/// ComParams:
/// - Byte 0: bits 4-2 = `CP_MessagePriority` (`PARAM_MESSAGE_PRIORITY`)
///   truncated to its 3-bit range; bit 0 = `CP_J1939DataPage`
///   (`PARAM_J1939_DATA_PAGE`)'s low bit. Bit 1 (Extended Data Page) and
///   the byte's top 3 bits are always zero -- this codebase has no EDP
///   ComParam (ADR-179's Context/Decision 6 do not mint one).
/// - Byte 1: the PDU Format, `CP_J1939PDUFormat` (`PARAM_J1939_PDU_FORMAT`),
///   used verbatim.
/// - Byte 2 (PDU Specific): PF >= 240 (PDU2, group-broadcast addressing)
///   uses `CP_J1939PDUSpecific` (`PARAM_J1939_PDU_SPECIFIC`, the group
///   extension) as-is; PF < 240 (PDU1, peer-to-peer addressing) uses the
///   destination address instead -- SAE J1939-21's own PDU1 addressing
///   rule -- the same `CP_J1939TargetAddress` value byte 4 below reads.
/// - Byte 3: `source_address`, the caller-supplied SAE J1939 source
///   address -- the CLL's currently claimed address, per ADR-179 Decision
///   3's state machine. Not resolved from ComParams by this function
///   itself: callers (`build_tx_message`) resolve it from wherever
///   Decision 3's claim result is recorded (currently the
///   `CP_TesterSourceAddress` Active-set writeback that claim loop
///   performs) and pass it in explicitly.
///
/// Byte 4 is always `CP_J1939TargetAddress` (`PARAM_J1939_TARGET_ADDRESS`)
/// truncated to a byte, `0xFF` selecting BAM (broadcast) segmentation per
/// clause 16.4.4. ADR-179 Decision 4 already rejects
/// `CP_J1939TargetAddress == 0xFFFF` at `StartComPrimitive` time, so a live
/// `CoptSendrecv`/`CoptStartcomm` message always sees a resolved
/// byte-range value here; the `0xFFFF -> 0xFF` truncation this function
/// would otherwise perform on an unresolved default is therefore
/// unreachable in practice, not a silent behavior choice.
pub(super) fn j1939_header_bytes(source_address: u8, active: &ComParamSet) -> Vec<u8> {
    let priority = active
        .unum32
        .get(&PARAM_MESSAGE_PRIORITY)
        .copied()
        .unwrap_or(6) as u8;
    let data_page = active
        .unum32
        .get(&PARAM_J1939_DATA_PAGE)
        .copied()
        .unwrap_or(0) as u8;
    let pdu_format = active
        .unum32
        .get(&PARAM_J1939_PDU_FORMAT)
        .copied()
        .unwrap_or(0) as u8;
    let pdu_specific = active
        .unum32
        .get(&PARAM_J1939_PDU_SPECIFIC)
        .copied()
        .unwrap_or(0) as u8;
    let target_address = active
        .unum32
        .get(&PARAM_J1939_TARGET_ADDRESS)
        .copied()
        .unwrap_or(0xFFFF) as u8;

    let byte0 = ((priority & 0x07) << 2) | (data_page & 0x01);
    let ps_byte = if pdu_format >= 240 {
        pdu_specific
    } else {
        target_address
    };
    vec![byte0, pdu_format, ps_byte, source_address, target_address]
}

/// ADR-200 (Phase 3): the RawMode=ON SAE J1939 TX shim `build_tx_message`'s
/// raw branch calls instead of the generic passthrough every other
/// RawMode-admitted protocol uses. `payload` is the client's own raw D-PDU
/// frame -- a 4-byte 29-bit CAN identifier (same byte layout
/// [`j1939_header_bytes`] composes: byte 0 priority/data-page, byte 1 PDU
/// Format, byte 2 PDU Specific, byte 3 source address) followed by the
/// message payload, per ISO 22900-2:2022 line 774/§10.1.4.19.5/Table 80.
/// This inserts the native destination-address (DA) byte SAE J2534-2
/// §16.4.3/Table 62 requires at wire position 4, mechanically derived from
/// the client's own CAN-ID bytes alone -- no ComParam is consulted, keeping
/// RawMode's "client owns addressing" premise intact:
///
/// - PF (`payload[1]`) < 240 (PDU1, peer-to-peer): DA = PS (`payload[2]`),
///   the same value [`j1939_header_bytes`]'s own PDU1 arm uses for its own
///   PDU-Specific byte -- SAE J1939-21's PDU1 addressing rule.
/// - PF >= 240 (PDU2, group-broadcast): DA = `0xFF` (BAM/broadcast
///   segmentation, clause 16.4.4).
///
/// Rejects (rather than panicking on an out-of-bounds slice) when `payload`
/// is shorter than 4 bytes -- there is no CAN-ID prefix to derive a DA from.
fn raw_j1939_tx_message(payload: &[u8]) -> Result<Vec<u8>, String> {
    if payload.len() < 4 {
        return Err(format!(
            "RawMode SAE J1939 cop_data must be at least 4 bytes (the 29-bit CAN ID, ISO \
             22900-2:2022 Table 80) so the native destination-address byte (SAE J2534-2 \
             §16.4.3/Table 62) can be derived from it -- got {} byte(s)",
            payload.len()
        ));
    }
    let pf = payload[1];
    let destination_address = if pf < 240 { payload[2] } else { 0xFF };
    let mut message = Vec::with_capacity(payload.len() + 1);
    message.extend_from_slice(&payload[..4]);
    message.push(destination_address);
    message.extend_from_slice(&payload[4..]);
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_with(params: &[(ComParamId, u32)]) -> EcuUniqueRespEntry {
        let mut set = ComParamSet::default();
        for &(id, value) in params {
            set.unum32.insert(id, value);
        }
        EcuUniqueRespEntry {
            unique_resp_identifier: 1,
            params: set,
        }
    }

    #[test]
    fn can_message_requires_unique_resp_id_table_entry() {
        let active = ComParamSet::default();
        let err = build_tx_message(
            ChannelProtocol::CAN,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x02, 0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.contains("CP_CanPhysReqId"), "{err}");
    }

    #[test]
    fn can_message_prepends_normal_addressing_can_id() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[(PARAM_CAN_PHYS_REQ_ID, 0x7E0)])];
        let message = build_tx_message(
            ChannelProtocol::CAN,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x02, 0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x00, 0x00, 0x07, 0xE0, 0x02, 0x10, 0x03]);
    }

    /// ADR-196 Decision item 2: `raw_mode = true` returns `payload`
    /// unchanged -- no CAN ID/header prefix construction at all -- even
    /// though `active` carries no `CP_CanPhysReqId` (which the non-RawMode
    /// arm above requires and would otherwise reject this exact call for).
    #[test]
    fn raw_mode_returns_payload_unchanged_with_no_header() {
        let active = ComParamSet::default();
        let payload = [0x00, 0x00, 0x07, 0xE0, 0x02, 0x10, 0x03];
        let message = build_tx_message(
            ChannelProtocol::CAN,
            AddrModeSource::Request,
            &active,
            &[],
            &payload,
            false,
            None,
            true,
        )
        .unwrap();
        assert_eq!(message, payload.to_vec());
    }

    #[test]
    fn iso15765_message_prepends_extended_addressing_can_id_and_ae() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x18DA01F1),
            (PARAM_CAN_PHYS_REQ_FORMAT, 0x08),
            (PARAM_CAN_PHYS_REQ_EXT_ADDR, 0xF1),
        ])];
        let message = build_tx_message(
            ChannelProtocol::ISO15765,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x18, 0xDA, 0x01, 0xF1, 0xF1, 0x10, 0x03]);
    }

    #[test]
    fn software_isotp_message_omits_ae_byte_even_under_extended_addressing() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x18DA01F1),
            (PARAM_CAN_PHYS_REQ_FORMAT, 0x08),
            (PARAM_CAN_PHYS_REQ_EXT_ADDR, 0xF1),
        ])];
        // events.rs::isotp_send always slices a fixed 4-byte CAN-ID offset off
        // this buffer; the AE byte is added per-frame by the poll task's own
        // frame builders from SoftIsoTpTx::tx_addressing, not by this buffer.
        let message = build_tx_message(
            ChannelProtocol::ISO15765,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x10, 0x03],
            true,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x18, 0xDA, 0x01, 0xF1, 0x10, 0x03]);
    }

    #[test]
    fn kwp_message_uses_entry_target_over_comparam_default() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x80);
        active.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x11)])];
        let message = build_tx_message(
            ChannelProtocol::ISO14230,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x22, 0x33],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x80, 0x11, 0xF1, 0x02, 0x22, 0x33]);
    }

    #[test]
    fn kwp_message_falls_back_to_comparam_target_with_no_table() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x80);
        active.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let message = build_tx_message(
            ChannelProtocol::ISO9141,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x01, 0x00],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x80, 0x10, 0xF1, 0x02, 0x01, 0x00]);
    }

    // ── ADR-166 regression tests (CARB/ISO9141-2 exception addressing and
    //    unaddressed/embedded-length KWP shapes) ────────────────────────────

    #[test]
    fn kwp_message_carb_format_has_no_trailing_length_byte() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x6C);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x11)])];

        let empty_payload = build_tx_message(
            ChannelProtocol::ISO9141,
            AddrModeSource::Request,
            &active,
            &entries,
            &[],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            empty_payload,
            vec![0x6C, 0x11, 0xF1],
            "CARB addressing (format & 0xC0 == 0x40) never has a trailing length byte, even \
             for an empty payload"
        );

        let nonempty_payload = build_tx_message(
            ChannelProtocol::ISO9141,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x22, 0x33],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            nonempty_payload,
            vec![0x6C, 0x11, 0xF1, 0x22, 0x33],
            "CARB addressing never has a trailing length byte, regardless of payload length"
        );
    }

    #[test]
    fn kwp_message_carb_functional_format_has_no_trailing_length_byte() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x02);
        active.unum32.insert(PARAM_FUNC_REQ_FORMAT_PRIORITY, 0x68);
        active.unum32.insert(PARAM_FUNC_REQ_TARGET_ADDR, 0x6A);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        // A configured UniqueRespIdTable entry must be ignored under functional addressing.
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x11)])];
        let message = build_tx_message(
            ChannelProtocol::ISO14230,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x22, 0x33],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            message,
            vec![0x68, 0x6A, 0xF1, 0x22, 0x33],
            "functional CARB addressing (format & 0xC0 == 0x40) never has a trailing length \
             byte either"
        );
    }

    #[test]
    fn kwp_message_unaddressed_format_has_no_target_or_source() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x00);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x11)])];

        let short = build_tx_message(
            ChannelProtocol::ISO9141,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0xAA],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(short, vec![0x00, 0x01, 0xAA]);

        let longer = build_tx_message(
            ChannelProtocol::ISO9141,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x11, 0x22, 0x33, 0x44],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(longer, vec![0x00, 0x04, 0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn kwp_message_embedded_length_recomposes_format_byte() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x81);
        active.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let message = build_tx_message(
            ChannelProtocol::ISO9141,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x22, 0x33],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            message,
            vec![0x82, 0x10, 0xF1, 0x22, 0x33],
            "the format byte's top 2 bits (0x80) are preserved, its low 6 bits are recomposed \
             to the actual payload length (2), and no separate length byte is appended"
        );
    }

    #[test]
    fn kwp_message_embedded_length_overflow_returns_err() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x81);
        active.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let payload = vec![0u8; 64];
        let err = build_tx_message(
            ChannelProtocol::ISO9141,
            AddrModeSource::Request,
            &active,
            &[],
            &payload,
            false,
            None,
            false,
        )
        .unwrap_err();
        assert!(
            err.contains("63-byte maximum"),
            "expected an Err describing the 63-byte embedded-length ceiling, got: {err}"
        );
    }

    #[test]
    fn kwp_message_embedded_length_exact_63_byte_boundary_succeeds() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x81);
        active.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let payload = vec![0xAAu8; 0x3F];
        let message = build_tx_message(
            ChannelProtocol::ISO9141,
            AddrModeSource::Request,
            &active,
            &[],
            &payload,
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            message[..3],
            [0x80 | 0x3F, 0x10, 0xF1],
            "63 (0x3F) is the exact ceiling of the 6-bit embedded length field and must still \
             recompose successfully, not overflow into the Err path"
        );
        assert_eq!(&message[3..], payload.as_slice());
    }

    #[test]
    fn kwp_message_embedded_length_mode_with_empty_payload_falls_back_to_explicit_length_byte() {
        // A zero-length payload can never be recomposed into the embedded
        // low-6-bits field: `0` there is indistinguishable on the wire from
        // "no length embedded -- a separate length byte follows"
        // (`kwp_header_and_payload_len`'s own parsing rule). This must fall
        // back to the explicit-length-byte shape (low 6 bits forced to `0`,
        // an explicit `0` length byte appended) rather than silently
        // dropping the length byte the recompose branch's bare 3-byte shape
        // would otherwise produce.
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x81);
        active.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let message = build_tx_message(
            ChannelProtocol::ISO9141,
            AddrModeSource::Request,
            &active,
            &[],
            &[],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            message,
            vec![0x80, 0x10, 0xF1, 0x00],
            "empty payload under a configured embedded-length format (0x81) must take the \
             explicit-length-byte shape (low 6 bits cleared to 0, explicit 0 length byte \
             appended), not recompose to a bare 3-byte header with the length silently dropped"
        );
    }

    #[test]
    fn j1850_message_has_no_length_or_checksum_byte() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x6C);
        active.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let message = build_tx_message(
            ChannelProtocol::J1850VPW,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x01, 0x00],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x6C, 0x10, 0xF1, 0x01, 0x00]);
    }

    #[test]
    fn j1850_message_falls_back_to_standard_obdii_priority_byte_per_protocol() {
        let active = ComParamSet::default();
        let vpw = build_tx_message(
            ChannelProtocol::J1850VPW,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x01, 0x00],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(vpw, vec![0x68, 0x10, 0xF1, 0x01, 0x00]);
        let pwm = build_tx_message(
            ChannelProtocol::J1850PWM,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x01, 0x00],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(pwm, vec![0x61, 0x10, 0xF1, 0x01, 0x00]);
    }

    #[test]
    fn sci_message_is_passthrough() {
        let active = ComParamSet::default();
        let message = build_tx_message(
            ChannelProtocol::SCI_A_ENGINE,
            AddrModeSource::Request,
            &active,
            &[],
            &[0xAA, 0xBB],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0xAA, 0xBB]);
    }

    #[test]
    fn can_message_requires_can_func_req_id_under_functional_addressing() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x02);
        let entries = [entry_with(&[(PARAM_CAN_PHYS_REQ_ID, 0x7E0)])];
        let err = build_tx_message(
            ChannelProtocol::CAN,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x02, 0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.contains("CP_CanFuncReqId"), "{err}");
    }

    #[test]
    fn can_message_uses_func_req_id_under_functional_addressing_ignoring_table() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x02);
        active.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
        // A configured UniqueRespIdTable entry must be ignored under functional addressing.
        let entries = [entry_with(&[(PARAM_CAN_PHYS_REQ_ID, 0x7E0)])];
        let message = build_tx_message(
            ChannelProtocol::CAN,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x02, 0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x00, 0x00, 0x07, 0xDF, 0x02, 0x10, 0x03]);
    }

    #[test]
    fn iso15765_functional_message_uses_extended_addressing_can_func_req_fields() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x02);
        active.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x18DB33F1);
        active.unum32.insert(PARAM_CAN_FUNC_REQ_FORMAT, 0x08);
        active.unum32.insert(PARAM_CAN_FUNC_REQ_EXT_ADDR, 0xFE);
        let message = build_tx_message(
            ChannelProtocol::ISO15765,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x18, 0xDB, 0x33, 0xF1, 0xFE, 0x10, 0x03]);
    }

    #[test]
    fn kwp_message_uses_func_req_fields_under_functional_addressing_ignoring_table() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x02);
        active.unum32.insert(PARAM_FUNC_REQ_FORMAT_PRIORITY, 0xC0);
        active.unum32.insert(PARAM_FUNC_REQ_TARGET_ADDR, 0x33);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        // A configured UniqueRespIdTable entry must be ignored under functional addressing.
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x11)])];
        let message = build_tx_message(
            ChannelProtocol::ISO14230,
            AddrModeSource::Request,
            &active,
            &entries,
            &[0x22, 0x33],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0xC0, 0x33, 0xF1, 0x02, 0x22, 0x33]);
    }

    #[test]
    fn j1850_message_uses_func_req_fields_under_functional_addressing() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x02);
        active.unum32.insert(PARAM_FUNC_REQ_FORMAT_PRIORITY, 0x68);
        active.unum32.insert(PARAM_FUNC_REQ_TARGET_ADDR, 0x6A);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let message = build_tx_message(
            ChannelProtocol::J1850VPW,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x01, 0x00],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x68, 0x6A, 0xF1, 0x01, 0x00]);
    }

    #[test]
    fn resolve_can_addressing_reports_functional_flag() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[(PARAM_CAN_PHYS_REQ_ID, 0x7E0)])];
        let physical = resolve_can_addressing(AddrModeSource::Request, &active, &entries).unwrap();
        assert!(!physical.functional);

        let mut functional_active = ComParamSet::default();
        functional_active
            .unum32
            .insert(PARAM_REQUEST_ADDR_MODE, 0x02);
        functional_active
            .unum32
            .insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
        let functional =
            resolve_can_addressing(AddrModeSource::Request, &functional_active, &entries).unwrap();
        assert!(functional.functional);
    }

    /// edge-case-hunter fix (UUDT/USDT sentinel follow-up): a sentinel-valued
    /// `CP_CanRespUSDTId` (`0xFFFFFFFF`, ISO 22900-2 Table 76's "not used"
    /// marker) must resolve `fc_can_id` to `None`, not `Some(0xFFFFFFFF)` --
    /// `SoftIsoTpTx.fc_can_id`'s own contract treats `None` as "accept any
    /// FlowControl frame" and `Some(id)` as "accept only `id`," so leaving
    /// the sentinel unfiltered would silently flip an unconfigured FC pairing
    /// into "accept no FC frame ever" (no real frame's CAN ID can equal
    /// `0xFFFFFFFF`), hanging every software-ISO-TP multi-frame TX on that
    /// link until N_bs timeout.
    #[test]
    fn resolve_can_addressing_ignores_sentinel_usdt_id_for_fc_can_id() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_USDT_ID, 0xFFFF_FFFF),
        ])];
        let addressing =
            resolve_can_addressing(AddrModeSource::Request, &active, &entries).unwrap();
        assert_eq!(
            addressing.fc_can_id, None,
            "a sentinel-valued CP_CanRespUSDTId must resolve to no FlowControl pairing, not a \
             literal 0xFFFFFFFF CAN ID"
        );
    }

    #[test]
    fn tester_present_addr_mode_selects_functional_can_addressing_independently() {
        let mut active = ComParamSet::default();
        // CP_RequestAddrMode left at its physical default; only
        // CP_TesterPresentAddrMode selects functional (ADR-138).
        active.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0x01);
        active.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
        let entries = [entry_with(&[(PARAM_CAN_PHYS_REQ_ID, 0x7E0)])];
        let message = build_tx_message(
            ChannelProtocol::CAN,
            AddrModeSource::TesterPresent,
            &active,
            &entries,
            &[0x02, 0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x00, 0x00, 0x07, 0xDF, 0x02, 0x10, 0x03]);
    }

    #[test]
    fn tester_present_addr_mode_selects_functional_kwp_addressing_independently() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0x01);
        active.unum32.insert(PARAM_FUNC_REQ_FORMAT_PRIORITY, 0xC0);
        active.unum32.insert(PARAM_FUNC_REQ_TARGET_ADDR, 0x33);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        // A configured UniqueRespIdTable entry must be ignored under functional addressing.
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x11)])];
        let message = build_tx_message(
            ChannelProtocol::ISO14230,
            AddrModeSource::TesterPresent,
            &active,
            &entries,
            &[0x22, 0x33],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0xC0, 0x33, 0xF1, 0x02, 0x22, 0x33]);
    }

    #[test]
    fn tester_present_addr_mode_selects_functional_j1850_addressing_independently() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0x01);
        active.unum32.insert(PARAM_FUNC_REQ_FORMAT_PRIORITY, 0x68);
        active.unum32.insert(PARAM_FUNC_REQ_TARGET_ADDR, 0x6A);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        let message = build_tx_message(
            ChannelProtocol::J1850VPW,
            AddrModeSource::TesterPresent,
            &active,
            &[],
            &[0x01, 0x00],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0x68, 0x6A, 0xF1, 0x01, 0x00]);
    }

    #[test]
    fn request_functional_does_not_imply_tester_present_functional() {
        // CP_RequestAddrMode = 2 (functional), CP_TesterPresentAddrMode absent
        // (defaults physical): on the identical `active` set, `Request`
        // resolves functional while `TesterPresent` resolves physical —
        // the two ComParams are independent knobs (ADR-138).
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x02);
        active.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
        let entries = [entry_with(&[(PARAM_CAN_PHYS_REQ_ID, 0x7E0)])];

        let request = resolve_can_addressing(AddrModeSource::Request, &active, &entries).unwrap();
        assert!(request.functional);
        assert_eq!(request.req_id, 0x7DF);

        let tester_present =
            resolve_can_addressing(AddrModeSource::TesterPresent, &active, &entries).unwrap();
        assert!(!tester_present.functional);
        assert_eq!(tester_present.req_id, 0x7E0);
    }

    #[test]
    fn tester_present_functional_does_not_imply_request_functional() {
        // Converse: CP_TesterPresentAddrMode = 1 (functional),
        // CP_RequestAddrMode absent (defaults physical): on the identical
        // `active` set, `TesterPresent` resolves functional while `Request`
        // resolves physical.
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0x01);
        active.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
        let entries = [entry_with(&[(PARAM_CAN_PHYS_REQ_ID, 0x7E0)])];

        let tester_present =
            resolve_can_addressing(AddrModeSource::TesterPresent, &active, &entries).unwrap();
        assert!(tester_present.functional);
        assert_eq!(tester_present.req_id, 0x7DF);

        let request = resolve_can_addressing(AddrModeSource::Request, &active, &entries).unwrap();
        assert!(!request.functional);
        assert_eq!(request.req_id, 0x7E0);
    }

    #[test]
    fn tester_present_addr_mode_missing_can_func_req_id_names_the_comparam() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0x01);
        let entries = [entry_with(&[(PARAM_CAN_PHYS_REQ_ID, 0x7E0)])];
        let err = build_tx_message(
            ChannelProtocol::CAN,
            AddrModeSource::TesterPresent,
            &active,
            &entries,
            &[0x02, 0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.contains("CP_TesterPresentAddrMode"), "{err}");
        assert!(err.contains("CP_CanFuncReqId"), "{err}");
    }

    // ── response_header_bytes (ADR-165 Decision 3) ──────────────────────────

    #[test]
    fn response_header_bytes_iso15765_uses_resp_usdt_id_not_phys_req_id() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_USDT_ID, 0x7E8),
        ])];
        let (header, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO15765, &active, &entries, None).unwrap();
        // The ECU's own response id (0x7E8), not the tester's request id
        // (0x7E0) build_tx_message would use for the same entry.
        assert_eq!(header, 0x7E8_u32.to_be_bytes().to_vec());
        assert_eq!(
            mask,
            vec![0xFF; header.len()],
            "CAN header has no wildcard positions"
        );
    }

    #[test]
    fn response_header_bytes_can_appends_ae_byte_under_extended_addressing() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_USDT_ID, 0x18DAF110),
            (PARAM_CAN_RESP_USDT_FORMAT, 0x0A), // 29-bit (0x02) + extended addressing (0x08)
            (PARAM_CAN_RESP_USDT_EXT_ADDR, 0xF1),
        ])];
        let (header, mask, flags) =
            response_header_bytes(ChannelProtocol::ISO15765, &active, &entries, None).unwrap();
        let mut expected = 0x18DAF110_u32.to_be_bytes().to_vec();
        expected.push(0xF1);
        assert_eq!(header, expected);
        assert_eq!(
            mask,
            vec![0xFF; header.len()],
            "CAN header has no wildcard positions"
        );
        assert_eq!(
            flags,
            j2534_0404::TX_EXTENDED_ID | j2534_0404::ISO15765_ADDR_TYPE,
            "response-side 29-bit + extended addressing must set both flags"
        );
    }

    // ── Finding 1 regression tests (Codex review, ADR-165 PR #42) ───────────

    #[test]
    fn response_header_bytes_can_uses_resp_uudt_id_not_usdt_id() {
        // A plain (non-ISO15765) CAN link's preset configures UUDT response
        // addressing, never USDT (USDT/FlowControl is ISO15765-specific) --
        // only CP_CanRespUUDTId/_Format/_ExtAddr are set here.
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_UUDT_ID, 0x18DAF110),
            (PARAM_CAN_RESP_UUDT_FORMAT, 0x0A), // 29-bit (0x02) + extended addressing (0x08)
            (PARAM_CAN_RESP_UUDT_EXT_ADDR, 0xF1),
        ])];
        let (header, mask, flags) =
            response_header_bytes(ChannelProtocol::CAN, &active, &entries, None).unwrap();
        let mut expected = 0x18DAF110_u32.to_be_bytes().to_vec();
        expected.push(0xF1);
        assert_eq!(header, expected);
        assert_eq!(
            mask,
            vec![0xFF; header.len()],
            "CAN header has no wildcard positions"
        );
        // Finding D (Codex review, ADR-165 PR #42 round 3): ISO15765_ADDR_TYPE
        // must NOT be set for a raw-CAN link even under extended addressing
        // -- only TX_EXTENDED_ID (29-bit CAN ID) applies here. See the
        // dedicated regression test below for the full rationale.
        assert_eq!(flags, j2534_0404::TX_EXTENDED_ID);
    }

    // ── Finding D regression test (Codex review, ADR-165 PR #42 round 3) ────

    #[test]
    fn response_header_bytes_can_extended_addressing_never_sets_iso15765_addr_type() {
        // ISO15765_ADDR_TYPE is an ISO15765-only extended-addressing
        // indicator (SAE J2534-1 Table B.13); a conforming adapter may
        // reject a raw-CAN message that sets it (see rpc_link.rs's
        // CanIdFormat::tx_flags, Codex review PR #32, for the established
        // rule this mirrors). A plain CAN link's UUDT extended addressing
        // (CP_CanRespUUDTFormat bit 0x08 set) must still append the `ae`
        // address byte to the header, but must never set the flag.
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_UUDT_ID, 0x7E8),
            (PARAM_CAN_RESP_UUDT_FORMAT, 0x08), // 11-bit + extended addressing only
            (PARAM_CAN_RESP_UUDT_EXT_ADDR, 0xF1),
        ])];
        let (header, _mask, flags) =
            response_header_bytes(ChannelProtocol::CAN, &active, &entries, None).unwrap();
        let mut expected = 0x7E8_u32.to_be_bytes().to_vec();
        expected.push(0xF1);
        assert_eq!(
            header, expected,
            "the ae byte is still appended to the header"
        );
        assert_eq!(
            flags, 0,
            "ISO15765_ADDR_TYPE must never be set on a raw-CAN template"
        );
    }

    #[test]
    fn response_header_bytes_iso15765_extended_addressing_still_sets_iso15765_addr_type() {
        // Companion to the above: ISO15765's own extended addressing must
        // still set the flag (this codepath's original, correct behavior).
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_USDT_ID, 0x7E8),
            (PARAM_CAN_RESP_USDT_FORMAT, 0x08), // 11-bit + extended addressing only
            (PARAM_CAN_RESP_USDT_EXT_ADDR, 0xF1),
        ])];
        let (header, _mask, flags) =
            response_header_bytes(ChannelProtocol::ISO15765, &active, &entries, None).unwrap();
        let mut expected = 0x7E8_u32.to_be_bytes().to_vec();
        expected.push(0xF1);
        assert_eq!(header, expected);
        assert_eq!(
            flags,
            j2534_0404::ISO15765_ADDR_TYPE,
            "ISO15765 extended addressing must still set the flag"
        );
    }

    #[test]
    fn response_header_bytes_iso15765_requires_resp_usdt_id() {
        let active = ComParamSet::default();
        // CP_CanPhysReqId set (so build_tx_message would succeed) but no
        // paired CP_CanRespUSDTId -- this CLL has no known expected response
        // address for a repeat slot's mask/pattern to scope against.
        let entries = [entry_with(&[(PARAM_CAN_PHYS_REQ_ID, 0x7E0)])];
        let err =
            response_header_bytes(ChannelProtocol::ISO15765, &active, &entries, None).unwrap_err();
        assert!(err.contains("CP_CanRespUSDTId"), "{err}");
    }

    #[test]
    fn response_header_bytes_can_requires_resp_uudt_id() {
        let active = ComParamSet::default();
        // CP_CanPhysReqId set but no paired CP_CanRespUUDTId -- a plain CAN
        // link has no known expected response address for a repeat slot's
        // mask/pattern to scope against. A stray CP_CanRespUSDTId (ISO15765-
        // specific) must NOT satisfy this (the earlier bug this fixes).
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_USDT_ID, 0x7E8),
        ])];
        let err = response_header_bytes(ChannelProtocol::CAN, &active, &entries, None).unwrap_err();
        assert!(err.contains("CP_CanRespUUDTId"), "{err}");
    }

    // ── Codex review, ADR-165 PR #42 round 19 regression test ───────────────

    #[test]
    fn response_header_bytes_can_rejects_unused_sentinel_resp_uudt_id() {
        // ISO 22900-2 Table B.20's 0xFFFFFFFF sentinel for CP_CanRespUUDTId
        // means "not used" even though the key is present -- an unedited/
        // default UniqueRespIdTable entry (see comparam_defaults.rs's
        // several raw-CAN presets) carries exactly this value. Treating it
        // as a real address must not happen; this must fail the same way an
        // entirely-absent CP_CanRespUUDTId already does.
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_UUDT_ID, 0xFFFF_FFFF),
        ])];
        let err = response_header_bytes(ChannelProtocol::CAN, &active, &entries, None).unwrap_err();
        assert!(err.contains("CP_CanRespUUDTId"), "{err}");
    }

    #[test]
    fn response_header_bytes_iso15765_rejects_unused_sentinel_resp_usdt_id() {
        // Companion to the UUDT test above: ISO 22900-2 Table 76 documents
        // the identical 0xFFFFFFFF "not used" sentinel for CP_CanRespUSDTId
        // (round 19 follow-up -- the earlier claim that USDT has no
        // analogous sentinel was wrong; comparam_defaults.rs's presets never
        // happening to seed it with the sentinel is an authoring convention,
        // not something SetUniqueRespIdTable enforces, so a client can still
        // legitimately stage it). Must fail the same way an entirely-absent
        // CP_CanRespUSDTId already does (see
        // response_header_bytes_iso15765_requires_resp_usdt_id above); a
        // real USDT value already resolves correctly per
        // response_header_bytes_iso15765_extended_addressing_still_sets_iso15765_addr_type.
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_USDT_ID, 0xFFFF_FFFF),
        ])];
        let err =
            response_header_bytes(ChannelProtocol::ISO15765, &active, &entries, None).unwrap_err();
        assert!(err.contains("CP_CanRespUSDTId"), "{err}");
    }

    #[test]
    fn response_header_bytes_can_requires_unique_resp_id_table_entry() {
        let active = ComParamSet::default();
        let err = response_header_bytes(ChannelProtocol::CAN, &active, &[], None).unwrap_err();
        assert!(err.contains("CP_CanPhysReqId"), "{err}");
    }

    // ── Finding 2 regression test (Codex review, ADR-165 PR #42) ────────────

    #[test]
    fn response_header_bytes_tx_flags_reflect_response_addressing_not_request() {
        // Request side: 11-bit, normal (non-extended) addressing --
        // can_addressing_tx_flags must report no flags for it.
        // Response side (CP_CanRespUSDTFormat/_ExtAddr): 29-bit + extended
        // addressing -- response_header_bytes's own flags must reflect this,
        // not the request's.
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_PHYS_REQ_FORMAT, 0x00),
            (PARAM_CAN_RESP_USDT_ID, 0x18DAF110),
            (PARAM_CAN_RESP_USDT_FORMAT, 0x0A),
            (PARAM_CAN_RESP_USDT_EXT_ADDR, 0xF1),
        ])];

        let request_addressing = resolve_can_addressing(AddrModeSource::Request, &active, &entries);
        let request_flags = can_addressing_tx_flags(request_addressing, j2534_0404::ISO15765);
        assert_eq!(
            request_flags, 0,
            "the request side is 11-bit/normal addressing -- no flags expected"
        );

        let (_header, _mask, response_flags) =
            response_header_bytes(ChannelProtocol::ISO15765, &active, &entries, None).unwrap();
        assert_eq!(
            response_flags,
            j2534_0404::TX_EXTENDED_ID | j2534_0404::ISO15765_ADDR_TYPE,
            "the response side is 29-bit/extended addressing -- the mask/pattern's TxFlags \
             must reflect this, not the request's (0) flags"
        );
    }

    /// Backlog fix (Codex review, PR #42 round 5 Finding K investigation):
    /// `can_addressing_tx_flags` must never report `ISO15765_ADDR_TYPE` for a
    /// plain raw-CAN link, even when its physical request ComParams configure
    /// extended addressing (`CP_CanPhysReqFormat`'s extended-addressing bit)
    /// -- per SAE J2534-1 Table B.13 that bit is ISO15765-only. `TX_EXTENDED_ID`
    /// (CAN ID width) is unaffected by the protocol gate.
    #[test]
    fn can_addressing_tx_flags_gates_iso15765_addr_type_on_protocol() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_PHYS_REQ_FORMAT, 0x0A), // 29-bit + extended addressing
            (PARAM_CAN_PHYS_REQ_EXT_ADDR, 0xF1),
        ])];
        let addressing = resolve_can_addressing(AddrModeSource::Request, &active, &entries);

        assert_eq!(
            can_addressing_tx_flags(addressing, j2534_0404::CAN),
            j2534_0404::TX_EXTENDED_ID,
            "a raw-CAN link must never carry ISO15765_ADDR_TYPE, regardless of the \
             ComParam-configured addressing width"
        );
        assert_eq!(
            can_addressing_tx_flags(addressing, j2534_0404::ISO15765),
            j2534_0404::TX_EXTENDED_ID | j2534_0404::ISO15765_ADDR_TYPE,
            "the identical addressing on an ISO15765 link must still carry both flags"
        );
        assert_eq!(
            can_addressing_tx_flags(addressing, j2534_0404::PROTOCOL_FD_ISO15765_PS),
            j2534_0404::TX_EXTENDED_ID | j2534_0404::ISO15765_ADDR_TYPE,
            "an FD_ISO15765_PS-qualified link (ADR-159) must count as ISO15765-family too"
        );
    }

    #[test]
    fn response_header_bytes_kwp_swaps_target_and_source_from_the_request_header() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let request = kwp_header_bytes(AddrModeSource::Request, &active, &entries, 3).unwrap();
        let (response, mask, flags) =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap();
        // Request: [format, target=ECU(0x10), source=tester(0xF1), len].
        assert_eq!(request[..3], [0x80, 0x10, 0xF1]);
        // Response: same format byte, but target/source swapped (this ECU is
        // now the source, the tester is now the target), plus a trailing
        // wildcarded length-byte placeholder -- see the Finding B regression
        // test below for why that 4th byte must be present.
        assert_eq!(response, vec![0x80, 0xF1, 0x10, 0]);
        assert_eq!(mask, vec![0xFF, 0xFF, 0xFF, 0x00]);
        assert_eq!(
            flags, 0,
            "KWP has no CAN ID width/extended-addressing concept"
        );
    }

    // ── Finding B regression tests (Codex review, ADR-165 PR #42 round 2) ───

    #[test]
    fn response_header_bytes_kwp_default_format_appends_wildcarded_length_byte() {
        // The default response format (0x80, used when
        // CP_PhysRespFormatPriorityType is unset) has embedded LEN bits
        // (format & 0x3F) == 0 -- SAE J2534-2/ISO14230-1's own encoding for
        // "a separate length byte follows target/source on the wire"
        // (kwp_header_and_payload_len, events.rs). Before this fix,
        // response_header_bytes silently omitted that 4th wire byte,
        // shifting every mask/pattern byte the client supplies one position
        // to the left of where it actually lands on a real response frame.
        let active = ComParamSet::default();
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (header, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap();
        assert_eq!(
            header.len(),
            4,
            "the real wire header is 4 bytes (format, target, source, length) for the \
             default 0x80 format -- omitting the length byte misaligns everything after it"
        );
        assert_eq!(mask.len(), header.len());
        // The fixed identity bytes (format/target/source) must still match
        // exactly; only the length-byte position (index 3) is a wildcard.
        assert_eq!(&mask[..3], &[0xFF, 0xFF, 0xFF]);
        assert_eq!(
            mask[3], 0x00,
            "the length byte's real value is payload-length-dependent and unpredictable \
             here -- it must be a don't-care position, not forced to match a specific value"
        );
        // This codebase's zero-mask convention (data[i] & 0x00 == pattern[i])
        // is only trivially satisfiable when pattern[i] == 0.
        assert_eq!(header[3], 0);

        // A client's own first mask/pattern payload byte must land at wire
        // offset 4 (right after the 4-byte header), not offset 3.
        let client_mask = [0xFFu8];
        let client_pattern = [0x41u8];
        let mut full_mask = mask.clone();
        full_mask.extend_from_slice(&client_mask);
        let mut full_pattern = header.clone();
        full_pattern.extend_from_slice(&client_pattern);
        assert_eq!(full_mask.len(), 5);
        assert_eq!(full_pattern.len(), 5);
        assert_eq!(
            full_mask[4], 0xFF,
            "the client's own mask byte belongs at index 4"
        );
        assert_eq!(
            full_pattern[4], 0x41,
            "the client's own pattern byte belongs at index 4, not shifted into the \
             length-byte slot at index 3"
        );
    }

    #[test]
    fn response_header_bytes_kwp_nonzero_embedded_len_has_no_length_byte() {
        // format & 0x3F != 0 means the payload length is embedded in the
        // format byte itself -- no separate length byte on the wire, so no
        // 4th header byte. Header length stays 3.
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x81);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (header, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap();
        assert_eq!(header.len(), 3);
        assert_eq!(&header[1..], &[0xF1, 0x10]);
        assert_eq!(&mask[1..], &[0xFF, 0xFF]);
    }

    /// Backlog fix (round 20 sibling gap to Finding B): the embedded-length
    /// case's format byte position must wildcard its low 6 bits (the real
    /// ECU response's embedded LEN value is payload-length-dependent and
    /// unpredictable from ComParams alone), while the top 2 bits (the
    /// `0x80`/`0xC0` physical/functional address-mode discriminator) stay an
    /// exact match.
    #[test]
    fn response_header_bytes_kwp_embedded_len_wildcards_low_six_bits_of_format_byte() {
        let mut active = ComParamSet::default();
        // 0x81 = 0x80 (physical, addressed) | 0x01 (embedded LEN = 1).
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x81);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (header, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap();
        assert_eq!(
            mask[0], 0xC0,
            "the format byte's low 6 bits (the embedded LEN value) must be a don't-care \
             position -- only the top-2-bit address-mode discriminator stays exact-match"
        );
        assert_eq!(
            header[0], 0x80,
            "this codebase's zero-mask convention (data[i] & mask[i] == pattern[i]) requires \
             the pattern's own low 6 bits to be zeroed wherever the mask wildcards them"
        );
    }

    #[test]
    fn response_header_bytes_iso14230_default_format_also_appends_wildcarded_length_byte() {
        // ISO14230 shares KWP's exact framing convention
        // (kwp_header_and_payload_len covers both protocols identically,
        // events.rs) -- this must not be an ISO9141-only fix.
        let active = ComParamSet::default();
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (header, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO14230, &active, &entries, None).unwrap();
        assert_eq!(header, vec![0x80, 0xF1, 0x10, 0]);
        assert_eq!(mask, vec![0xFF, 0xFF, 0xFF, 0x00]);
    }

    #[test]
    fn response_header_bytes_j1850_swaps_target_and_source_from_the_request_header() {
        let active = ComParamSet::default();
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let request = j1850_header_bytes(
            j2534_0404::J1850VPW,
            AddrModeSource::Request,
            &active,
            &entries,
        );
        let (response, mask, flags) =
            response_header_bytes(ChannelProtocol::J1850VPW, &active, &entries, None).unwrap();
        assert_eq!(request, vec![0x68, 0x10, 0xF1]);
        assert_eq!(response, vec![0x68, 0xF1, 0x10]);
        assert_eq!(
            mask,
            vec![0xFF, 0xFF, 0xFF],
            "J1850 has no separate-length-byte concept -- no wildcard position"
        );
        assert_eq!(
            flags, 0,
            "J1850 has no CAN ID width/extended-addressing concept"
        );
    }

    // ── Bug 2 regression tests (edge-case-hunter, ADR-165) ──────────────────

    #[test]
    fn response_header_bytes_kwp_uses_resp_format_not_req_format() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x80);
        // Functional (0xC0 top bits), embedded LEN = 1 -- distinguishable
        // from the request's physical (0x80) format even after the
        // embedded-length wildcard fix masks the low 6 bits (0xC1 -> 0xC0).
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0xC1);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let request = kwp_header_bytes(AddrModeSource::Request, &active, &entries, 3).unwrap();
        let (response, _mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap();
        // The request's format byte is CP_PhysReqFormatPriorityType (0x80);
        // the response's must be resolved from the distinct
        // CP_PhysRespFormatPriorityType (0xC1, masked to 0xC0 by the
        // embedded-length wildcard fix), not a copy of the request's.
        assert_eq!(request[0], 0x80);
        assert_eq!(response[0], 0xC0);
    }

    // ── ADR-166 regression test (CARB/ISO9141-2 exception addressing) ───────

    #[test]
    fn response_header_bytes_kwp_carb_format_has_no_trailing_length_byte() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x6C);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (response, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap();
        assert_eq!(
            response,
            vec![0x6C, 0xF1, 0x10],
            "CARB addressing (format & 0xC0 == 0x40) is always a 3-byte header with no \
             trailing length byte"
        );
        assert_eq!(
            mask,
            vec![0xFF, 0xFF, 0xFF],
            "unlike the 0x80 default-format case, there is no wildcarded length-byte position"
        );
    }

    // ── Finding 2 regression tests (Codex review, ADR-165 PR #42 round 11) ──

    /// `CP_PhysRespFormatPriorityType` is `PDU_PC_UNIQUE_ID`-scoped (per-ECU),
    /// exactly like `CP_EcuRespSourceAddress` (`ecu_addr`'s own param) --
    /// when the selected UniqueRespIdTable entry sets it, that value must win
    /// over whatever the common Active ComParamSet happens to hold, mirroring
    /// `ecu_addr`'s already-established entries-first-then-active fallback.
    #[test]
    fn response_header_bytes_kwp_uses_entrys_own_format_over_active_set() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x80);
        // Functional (0xC0 top bits), embedded LEN = 1 -- distinguishable
        // from the active set's physical (0x80) format even after the
        // embedded-length wildcard fix masks the low 6 bits (0xC1 -> 0xC0).
        let entries = [entry_with(&[
            (PARAM_ECU_RESP_SOURCE_ADDR, 0x10),
            (PARAM_PHYS_RESP_FORMAT_PRIORITY, 0xC1),
        ])];
        let (response, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap();
        // The entry's own 0xC1 (functional, embedded LEN bits nonzero -- no
        // length byte) must be used, not the active set's 0x80 physical
        // (which would append a wildcarded length byte instead, a 4-byte
        // header, and a different top-2-bit address-mode discriminator).
        assert_eq!(
            response,
            vec![0xC0, 0xF1, 0x10],
            "the per-ECU UniqueRespIdTable entry's own CP_PhysRespFormatPriorityType must win \
             over the common Active ComParamSet's value (0xC1 masked to 0xC0 by the \
             embedded-length wildcard, not active's 0x80)"
        );
        assert_eq!(mask, vec![0xC0, 0xFF, 0xFF]);
    }

    /// Companion to the above: when the selected entry does NOT set
    /// `CP_PhysRespFormatPriorityType`, resolution falls back to the Active
    /// ComParamSet exactly as before this fix -- the fallback chain, not just
    /// the entry-first read, must be preserved.
    #[test]
    fn response_header_bytes_iso14230_falls_back_to_active_set_when_entry_has_no_format() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x81);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (response, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO14230, &active, &entries, None).unwrap();
        // 0x81's embedded LEN bits (0x01) are wildcarded by the
        // embedded-length fix -- header[0] masked to 0x80, mask[0] = 0xC0.
        assert_eq!(response, vec![0x80, 0xF1, 0x10]);
        assert_eq!(mask, vec![0xC0, 0xFF, 0xFF]);
    }

    // ── round-16 regression tests (Codex review, ADR-165 PR #42 round 16) ──

    /// An unaddressed KWP/ISO14230 response format (top 2 bits of
    /// `CP_PhysRespFormatPriorityType` == `0x00`) carries no tester/ECU
    /// address pair on the wire at all -- `response_header_bytes` must not
    /// append the two address bytes in that case, mirroring
    /// `events.rs::kwp_header_and_payload_len`'s own `has_addr` gate. Per
    /// ADR-166, unaddressed mode's trailing length byte is unconditional
    /// (present regardless of the low 6 bits) -- unlike the addressed
    /// `0x80`/`0xC0` modes, there is no embedded-length variant here, so this
    /// test (unlike its round-16 predecessor) does not attempt to isolate
    /// the address-byte gate from the length-byte one.
    #[test]
    fn response_header_bytes_kwp_unaddressed_format_has_no_address_bytes() {
        let mut active = ComParamSet::default();
        // Top 2 bits clear (unaddressed); low 6 bits nonzero -- under
        // ADR-166's corrected model this has no bearing on the trailing
        // length byte, which unaddressed mode always carries.
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x01);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (header, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap();
        assert_eq!(
            header,
            vec![0x01, 0],
            "an unaddressed response format's wire header carries no tester/ECU address bytes, \
             but always carries a trailing (wildcarded) length byte (ADR-166)"
        );
        assert_eq!(mask, vec![0xFF, 0x00]);
    }

    /// Companion regression guard: an addressed format (bit 7 set) must
    /// still get both address bytes exactly as before this fix.
    #[test]
    fn response_header_bytes_iso14230_addressed_format_still_has_address_bytes() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x81);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (header, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO14230, &active, &entries, None).unwrap();
        assert_eq!(
            header,
            // 0x81's embedded LEN bits (0x01) are wildcarded by the
            // embedded-length fix -- header[0] masked to 0x80.
            vec![0x80, 0xF1, 0x10],
            "an addressed response format's header must still carry both tester/ECU address \
             bytes -- this must not regress the pre-existing addressed case"
        );
        assert_eq!(mask, vec![0xC0, 0xFF, 0xFF]);
    }

    /// Both round-16 (address-byte gating) and round-2 Finding B
    /// (length-byte placeholder) apply simultaneously: an unaddressed format
    /// (bit 7 clear) whose low 6 bits are ALSO zero (embedded LEN unset, so
    /// a separate trailing length byte is on the wire too). Confirms the
    /// length-byte placeholder lands at the correct position (`1`, right
    /// after the format byte) once the address bytes are absent, not at the
    /// position it would occupy if they were present (`3`).
    #[test]
    fn response_header_bytes_kwp_unaddressed_format_with_embedded_len_zero_composes_correctly() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x00);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (header, mask, _flags) =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap();
        assert_eq!(
            header,
            vec![0x00, 0],
            "no address bytes (bit 7 clear) but a wildcarded trailing length-byte placeholder \
             (low 6 bits zero) -- the length byte must sit immediately after the format byte, \
             not after two nonexistent address bytes"
        );
        assert_eq!(mask, vec![0xFF, 0x00]);
    }

    #[test]
    fn response_header_bytes_j1850_uses_resp_format_not_req_format() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x6C);
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x2C);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let request = j1850_header_bytes(
            j2534_0404::J1850VPW,
            AddrModeSource::Request,
            &active,
            &entries,
        );
        let (response, _mask, _flags) =
            response_header_bytes(ChannelProtocol::J1850VPW, &active, &entries, None).unwrap();
        // Mirrors iso_15031_5_on_sae_j1850_vpw's real preset split
        // (phys_format 0x6C vs phys_resp_format 0x2C, comparam_defaults.rs).
        assert_eq!(request[0], 0x6C);
        assert_eq!(response[0], 0x2C);
    }

    // ── Finding 1 regression test (Codex review, ADR-165 PR #42 round 12) ──

    /// Companion to `response_header_bytes_kwp_uses_entrys_own_format_over_active_set`
    /// (round 11): `CP_PhysRespFormatPriorityType` is `PDU_PC_UNIQUE_ID`-scoped
    /// regardless of protocol family, so the J1850 branch needs the identical
    /// entries-first-then-active fallback the KWP branch already has.
    #[test]
    fn response_header_bytes_j1850_uses_entrys_own_format_over_active_set() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x68);
        let entries = [entry_with(&[
            (PARAM_ECU_RESP_SOURCE_ADDR, 0x10),
            (PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x2C),
        ])];
        let (response, mask, _flags) =
            response_header_bytes(ChannelProtocol::J1850VPW, &active, &entries, None).unwrap();
        assert_eq!(
            response,
            vec![0x2C, 0xF1, 0x10],
            "the per-ECU UniqueRespIdTable entry's own CP_PhysRespFormatPriorityType must win \
             over the common Active ComParamSet's value"
        );
        assert_eq!(mask, vec![0xFF, 0xFF, 0xFF]);
    }

    /// Companion to the above: when the selected entry does NOT set
    /// `CP_PhysRespFormatPriorityType`, resolution falls back to the Active
    /// ComParamSet; when neither sets it, J1850's own protocol-specific
    /// `default_format` (not KWP's hardcoded `0x80`) is the final fallback.
    #[test]
    fn response_header_bytes_j1850_falls_back_to_active_set_then_default_format() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x2C);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let (response, _mask, _flags) =
            response_header_bytes(ChannelProtocol::J1850VPW, &active, &entries, None).unwrap();
        assert_eq!(response[0], 0x2C, "falls back to the Active ComParamSet");

        let active_empty = ComParamSet::default();
        let (response_default, _mask, _flags) =
            response_header_bytes(ChannelProtocol::J1850VPW, &active_empty, &entries, None)
                .unwrap();
        assert_eq!(
            response_default[0], 0x68,
            "falls back to J1850VPW's own default_format (0x68), not KWP's hardcoded 0x80"
        );
    }

    #[test]
    fn response_header_bytes_kwp_rejects_functional_addressing() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_REQUEST_ADDR_MODE, 2);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let err =
            response_header_bytes(ChannelProtocol::ISO9141, &active, &entries, None).unwrap_err();
        assert!(err.contains("no functional"), "{err}");
    }

    #[test]
    fn response_header_bytes_j1850_rejects_functional_addressing() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_REQUEST_ADDR_MODE, 2);
        let entries = [entry_with(&[(PARAM_ECU_RESP_SOURCE_ADDR, 0x10)])];
        let err =
            response_header_bytes(ChannelProtocol::J1850VPW, &active, &entries, None).unwrap_err();
        assert!(err.contains("no functional"), "{err}");
    }

    #[test]
    fn response_header_bytes_sci_is_empty() {
        let active = ComParamSet::default();
        let (header, mask, flags) =
            response_header_bytes(ChannelProtocol::SCI_A_ENGINE, &active, &[], None).unwrap();
        assert!(header.is_empty());
        assert!(mask.is_empty());
        assert_eq!(flags, 0);
    }

    // ── ADR-179/Phase 5: SAE J1939 message framing (`j1939_header_bytes`) ────

    #[test]
    fn j1939_header_bytes_pdu1_uses_target_address_as_ps_byte() {
        // PF < 240 (PDU1, peer-to-peer): the PS byte (byte 2) is the
        // destination address, the SAME value byte 4 carries -- not
        // CP_J1939PDUSpecific.
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_MESSAGE_PRIORITY, 6);
        active.unum32.insert(PARAM_J1939_DATA_PAGE, 0);
        active.unum32.insert(PARAM_J1939_PDU_FORMAT, 0xEA); // 234, < 240
        active.unum32.insert(PARAM_J1939_PDU_SPECIFIC, 0x55); // must be ignored
        active.unum32.insert(PARAM_J1939_TARGET_ADDRESS, 0x21);

        let header = j1939_header_bytes(0x80, &active);

        // byte0 = (priority(6) & 0x7) << 2 | (data_page(0) & 1) = 0x18
        assert_eq!(
            header,
            vec![0x18, 0xEA, 0x21, 0x80, 0x21],
            "PDU1 (PF<240): PS byte (index 2) equals the destination address \
             (index 4), CP_J1939PDUSpecific is not consulted"
        );
    }

    #[test]
    fn j1939_header_bytes_pdu2_uses_pdu_specific_as_ps_byte() {
        // PF >= 240 (PDU2, group broadcast): the PS byte (byte 2) is
        // CP_J1939PDUSpecific (the group extension) as-is, independent of
        // the destination address in byte 4.
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_MESSAGE_PRIORITY, 3);
        active.unum32.insert(PARAM_J1939_DATA_PAGE, 1);
        active.unum32.insert(PARAM_J1939_PDU_FORMAT, 0xF0); // 240, >= 240
        active.unum32.insert(PARAM_J1939_PDU_SPECIFIC, 0x04);
        active.unum32.insert(PARAM_J1939_TARGET_ADDRESS, 0xFF); // BAM

        let header = j1939_header_bytes(0x17, &active);

        // byte0 = (priority(3) & 0x7) << 2 | (data_page(1) & 1) = 0x0D
        assert_eq!(
            header,
            vec![0x0D, 0xF0, 0x04, 0x17, 0xFF],
            "PDU2 (PF>=240): PS byte (index 2) equals CP_J1939PDUSpecific, \
             not the destination address"
        );
    }

    #[test]
    fn j1939_header_bytes_boundary_pf_239_is_still_pdu1() {
        // PF == 239 is the last PDU1 value (< 240); confirms the >= 240
        // boundary is exact, not off-by-one.
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_J1939_PDU_FORMAT, 239);
        active.unum32.insert(PARAM_J1939_PDU_SPECIFIC, 0x99);
        active.unum32.insert(PARAM_J1939_TARGET_ADDRESS, 0x0A);

        let header = j1939_header_bytes(0x01, &active);
        assert_eq!(
            header[2], 0x0A,
            "PF=239 must still resolve to PDU1 (target address)"
        );
    }

    #[test]
    fn j1939_header_bytes_top_bits_of_byte0_are_always_zero() {
        // Data[0] carries only CAN ID bits 28-24; the top 3 bits of the
        // byte must always be zero regardless of an out-of-range priority
        // value.
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_MESSAGE_PRIORITY, 0xFF); // out of range
        active.unum32.insert(PARAM_J1939_DATA_PAGE, 0xFF); // out of range
        active.unum32.insert(PARAM_J1939_PDU_FORMAT, 0);
        active.unum32.insert(PARAM_J1939_TARGET_ADDRESS, 0);

        let header = j1939_header_bytes(0, &active);
        assert_eq!(
            header[0] & 0xE0,
            0,
            "top 3 bits of byte 0 must always be zero"
        );
    }

    #[test]
    fn j1939_header_bytes_defaults_when_comparams_absent() {
        let active = ComParamSet::default();
        let header = j1939_header_bytes(0xF1, &active);
        // priority default 6, data_page default 0, PF default 0 (PDU1),
        // target_address default 0xFFFF truncated to 0xFF.
        assert_eq!(header, vec![0x18, 0x00, 0xFF, 0xF1, 0xFF]);
    }

    #[test]
    fn build_tx_message_j1939_prepends_five_byte_prefix() {
        let mut active = ComParamSet::default();
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0x80); // claimed SA writeback target
        active.unum32.insert(PARAM_MESSAGE_PRIORITY, 6);
        active.unum32.insert(PARAM_J1939_PDU_FORMAT, 0xEA);
        active.unum32.insert(PARAM_J1939_TARGET_ADDRESS, 0x21);

        let message = build_tx_message(
            ChannelProtocol::J1939_PS,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x00, 0xF0, 0x04],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            message,
            vec![0x18, 0xEA, 0x21, 0x80, 0x21, 0x00, 0xF0, 0x04],
            "5-byte prefix (claimed SA from NODE_ADDRESS) precedes client payload"
        );
    }

    // ── ADR-200/Phase 3: RawMode SAE J1939 TX shim (`raw_j1939_tx_message`) ──

    /// PDU1 (PF < 240): the derived DA equals the client's own PS byte
    /// (`payload[2]`), matching SAE J1939-21's peer-to-peer addressing rule
    /// -- the same value [`j1939_header_bytes`]'s own PDU1 arm uses.
    #[test]
    fn raw_j1939_tx_message_pdu1_derives_da_from_ps_byte() {
        let payload = [0x18, 0x00, 0x21, 0x80, 0x01, 0x02];
        let message = build_tx_message(
            ChannelProtocol::J1939_PS,
            AddrModeSource::Request,
            &ComParamSet::default(),
            &[],
            &payload,
            false,
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            message,
            vec![0x18, 0x00, 0x21, 0x80, 0x21, 0x01, 0x02],
            "DA (byte 4) must equal the client's own PS byte (byte 2) for PDU1"
        );
    }

    /// PDU2 (PF >= 240): the derived DA is always `0xFF` (BAM/broadcast
    /// segmentation, clause 16.4.4), regardless of the client's own PS byte
    /// value.
    #[test]
    fn raw_j1939_tx_message_pdu2_derives_da_as_broadcast() {
        let payload = [0x18, 0xF0, 0x99, 0x80, 0x01, 0x02];
        let message = build_tx_message(
            ChannelProtocol::J1939_PS,
            AddrModeSource::Request,
            &ComParamSet::default(),
            &[],
            &payload,
            false,
            None,
            true,
        )
        .unwrap();
        assert_eq!(
            message,
            vec![0x18, 0xF0, 0x99, 0x80, 0xFF, 0x01, 0x02],
            "DA (byte 4) must be 0xFF for PDU2 regardless of the PS byte"
        );
    }

    /// A boundary PF of exactly 240 is PDU2 (`>= 240`), matching
    /// `j1939_header_bytes`'s own `pdu_format >= 240` PDU2 threshold.
    #[test]
    fn raw_j1939_tx_message_pdu_format_boundary_240_is_pdu2() {
        let payload = [0x00, 240, 0x00, 0x00];
        let message = build_tx_message(
            ChannelProtocol::J1939_PS,
            AddrModeSource::Request,
            &ComParamSet::default(),
            &[],
            &payload,
            false,
            None,
            true,
        )
        .unwrap();
        assert_eq!(message[4], 0xFF, "PF == 240 must resolve to PDU2 (DA=0xFF)");
    }

    /// A payload shorter than 4 bytes has no CAN-ID prefix to derive a DA
    /// from -- rejected, not panicking on an out-of-bounds slice.
    #[test]
    fn raw_j1939_tx_message_rejects_a_payload_shorter_than_four_bytes() {
        let err = build_tx_message(
            ChannelProtocol::J1939_PS,
            AddrModeSource::Request,
            &ComParamSet::default(),
            &[],
            &[0x00, 0x00, 0x00],
            false,
            None,
            true,
        )
        .unwrap_err();
        assert!(err.contains("4 bytes"), "{err}");
    }

    /// Every other RawMode-admitted protocol keeps the plain, protocol-
    /// agnostic passthrough (no DA insertion) -- proves the J1939 shim is
    /// scoped to J1939 alone, not accidentally applied elsewhere.
    #[test]
    fn raw_mode_can_still_returns_payload_unchanged_alongside_the_j1939_shim() {
        let payload = [0x00, 0x00, 0x07, 0xE0, 0x02, 0x10, 0x03];
        let message = build_tx_message(
            ChannelProtocol::CAN,
            AddrModeSource::Request,
            &ComParamSet::default(),
            &[],
            &payload,
            false,
            None,
            true,
        )
        .unwrap();
        assert_eq!(message, payload);
    }

    /// Codex review regression (PR #97, ADR-188 Fix A): the TP2.0 arm frames
    /// with the caller-supplied `tp20_established_tx_id`, never a ComParam --
    /// staging a bogus `CP_TP20TxIdProposal` on `active` must have no effect
    /// on the framed message.
    #[test]
    fn build_tx_message_tp20_prepends_the_supplied_established_tx_id() {
        let mut active = ComParamSet::default();
        // A client-staged value on the very ComParam the old, buggy arm used
        // to read from -- must be completely ignored now.
        active.unum32.insert(PARAM_TP20_TX_ID_PROPOSAL, 0xDEAD_BEEF);

        let message = build_tx_message(
            ChannelProtocol::TP2_0_PS,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x10, 0x03],
            false,
            Some(0x0000_0123),
            false,
        )
        .unwrap();
        assert_eq!(
            message,
            vec![0x00, 0x00, 0x01, 0x23, 0x10, 0x03],
            "4-byte established TX-ID (from the explicit parameter, not the \
             staged ComParam) precedes client payload"
        );
    }

    /// Codex review regression (PR #97, ADR-188 Fix A): `None` -- no
    /// established TP2.0 connection for this CLL -- rejects with the
    /// existing no-connection error, regardless of what a client may have
    /// staged on `CP_TP20TxIdProposal`.
    #[test]
    fn build_tx_message_tp20_rejects_when_not_established() {
        let mut active = ComParamSet::default();
        // A client-staged bogus proposal must not substitute for a real
        // established connection (the actual Codex-review bug this fix
        // closes).
        active.unum32.insert(PARAM_TP20_TX_ID_PROPOSAL, 0x0000_0321);

        let err = build_tx_message(
            ChannelProtocol::TP2_0_PS,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap_err();
        assert!(err.contains("established connection"), "{err}");
    }

    /// ADR-192/Phase 7 Stage 7c: a staged `CP_TP20BroadcastAddress` composes
    /// `[address] ++ payload` directly, never the 4-byte established-TX-ID
    /// prefix.
    #[test]
    fn build_tx_message_tp20_broadcast_composes_address_then_payload() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, 0xF3);

        let message = build_tx_message(
            ChannelProtocol::TP2_0_PS,
            AddrModeSource::Request,
            &active,
            &[],
            &[0x10, 0x03],
            false,
            None,
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0xF3, 0x10, 0x03]);
    }

    /// ADR-192/Phase 7 Stage 7c Decision item 1: a broadcast send has no
    /// Established-connection precondition -- it must succeed even when
    /// `tp20_established_tx_id` is `None` (no TP2.0 connection ever started
    /// on this CLL), unlike an ordinary connection-bound TP2.0 send (see
    /// `build_tx_message_tp20_rejects_when_not_established` just above).
    #[test]
    fn build_tx_message_tp20_broadcast_succeeds_with_no_connection_established() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, 0xF0);

        let message = build_tx_message(
            ChannelProtocol::TP2_0_PS,
            AddrModeSource::Request,
            &active,
            &[],
            &[0xAA, 0x55],
            false,
            None, // no established connection -- must not matter for a broadcast
            false,
        )
        .unwrap();
        assert_eq!(message, vec![0xF0, 0xAA, 0x55]);
    }

    /// Codex review regression (PR #97, ADR-188 Fix H): the incoming frame's
    /// leading 4 bytes on a TP2.0 CLL are the RX-ID header (clause 19.4.4
    /// Table 81), not payload -- `response_header_bytes` must prefix the
    /// repeat-slot stop-condition mask/pattern with an exact-match template
    /// for those 4 bytes, mirroring `build_tx_message_tp20_prepends_the_
    /// supplied_established_tx_id`'s TX-ID counterpart above.
    #[test]
    fn response_header_bytes_tp20_prepends_the_supplied_established_rx_id() {
        let active = ComParamSet::default();
        let (header, mask, tx_flags) =
            response_header_bytes(ChannelProtocol::TP2_0_PS, &active, &[], Some(0x0000_0456))
                .unwrap();
        assert_eq!(
            header,
            vec![0x00, 0x00, 0x04, 0x56],
            "4-byte established RX-ID (from the explicit parameter, not a \
             ComParam) forms the header template"
        );
        assert_eq!(
            mask,
            vec![0xFF, 0xFF, 0xFF, 0xFF],
            "exact match required on all 4 bytes"
        );
        assert_eq!(tx_flags, 0);
    }

    /// Codex review regression (P2, PR #97, round 18): unlike the CAN/
    /// ISO15765 arm above, `response_header_bytes`'s TP2.0 arm returned `0`
    /// for `tx_flags` unconditionally, even when the established RX-ID
    /// requires a 29-bit CAN identifier -- the device would then evaluate
    /// the repeat-slot stop-condition template as an 11-bit template and
    /// might never recognize a match.
    #[test]
    fn response_header_bytes_tp20_sets_extended_id_when_rx_id_exceeds_11_bit_range() {
        let active = ComParamSet::default();
        let (_, _, tx_flags) =
            response_header_bytes(ChannelProtocol::TP2_0_PS, &active, &[], Some(0x1000_0321))
                .unwrap();
        assert_eq!(
            tx_flags,
            j2534_0404::TX_EXTENDED_ID,
            "an RX-ID above 0x7FF cannot be represented as an 11-bit CAN ID"
        );
    }

    /// Codex review regression (PR #97, ADR-188 Fix H): `None` -- no
    /// established TP2.0 connection for this CLL -- rejects rather than
    /// falling through to the empty-header default (the actual bug this fix
    /// closes: an empty header left the client's mask/pattern compared
    /// against the wrong wire bytes instead of being rejected outright).
    #[test]
    fn response_header_bytes_tp20_rejects_when_not_established() {
        let active = ComParamSet::default();
        let err = response_header_bytes(ChannelProtocol::TP2_0_PS, &active, &[], None).unwrap_err();
        assert!(err.contains("established connection"), "{err}");
    }
}
