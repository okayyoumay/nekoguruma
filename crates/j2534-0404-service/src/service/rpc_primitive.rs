use std::ops::RangeInclusive;

use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::Code;
use tracing::{debug, warn};
use vci_service_interface::PduError;

use super::{
    ExpectedResponse, comparam_support,
    rpc_link::{effective_fd_tx_dl, fd_can_tx_message_size_range, fd_mode_staged},
    rpc_misc::TakenBroadcastPeriodic,
    *,
};
use crate::error::{map_native_error_for_link, state_guard_status, unknown_handle_status};

/// Maximum size, in bytes, of a `StartComPrimitiveRequest.cop_tag` (ADR-204).
/// Generous enough for a UUID (16 bytes) or a compact string key; enforced in
/// `rpc_start_com_primitive` before any `CopEntry` is constructed, since the
/// tag is echoed on every COP-status event emitted for the resulting COP --
/// an unbounded tag would be a per-event amplification hazard. Not a literal
/// shared `const` with `iso22900-service`'s own `MAX_COP_TAG_LEN`
/// (`iso22900-service/src/service/rpc_primitive.rs`), but the two values
/// must agree, since both are the same documented wire contract
/// (`docs/rpc-api-guide.md`).
pub(super) const MAX_COP_TAG_LEN: usize = 64;

/// Maps proto `ComPrimitiveCtrlData.tx_flag` to a J2534 `TxFlags` u32.
///
/// Named bits are translated to the corresponding J2534 flag constant.
/// Raw bytes are the ISO 22900-2 D.2.1 (Table D.4) `TxFlag` byte-array
/// layout -- the same layout `PDU_COP_CTRL_DATA.TxFlag` uses natively in
/// `iso22900-service` (`convert.rs`'s `tx_flag_to_iso`, which passes the
/// bytes straight through to the D-PDU API) -- decoded bit-by-bit into the
/// corresponding J2534 `TxFlags` bit position (ADR-116), not byte-copied as
/// an already-native J2534 u32.
///
/// `TxFlagCan29bitId`/`TxFlagIso15765AddrType`, and their raw D.2.1
/// equivalents (byte 2 bit 0 / byte 3 bit 7), are deliberately not mapped
/// here UNLESS `raw_mode` is `true`: outside RawMode,
/// `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE` describe objective
/// facts about the CAN/ISO15765 addressing this service already resolved
/// from ComParams, not caller preference, so every call site applies
/// `tx_header::can_addressing_tx_flags` over whatever this function returns
/// for those two bit positions, superseding any value a client requests
/// here by either representation (ADR-062). D.2.1's
/// `SUPPRESS_POS_RESP`/`ENABLE_EXTRA_INFO` (byte 0 bits 6/5) have no J2534
/// `TxFlags` equivalent (SAE J2534-1 Figure 45) and are dropped.
///
/// **`raw_mode` (ADR-196 Decision item 2, extended by ADR-198 Phase 2):**
/// `true` for a RawMode=ON CLL. The protocol allowlist
/// (`rpc_link::rpc_create_com_logical_link`) accepts RawMode=ON for a base
/// CAN or hardware ISO15765 link, a hardware K-line (ISO9141/ISO14230)
/// link, AND, as a documented no-op exception, Analog Inputs/SCI -- so
/// `raw_mode` alone does NOT imply a CAN/ISO15765 link. `hw_protocol_id` gates the
/// `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE` passthrough below to CAN/ISO15765
/// specifically (`resources::base_protocol_id`-normalized): these are
/// CAN-specific bits with no meaning for SCI/Analog Inputs, and passing
/// them through unconditionally for those protocols would leak an invalid
/// native flag into their TX message and break Decision item 1's own
/// "RawMode ON/OFF are observably identical" no-op guarantee for them
/// (Codex review, PR #107 round 3). Once RawMode hands header construction
/// to the client on a CAN/ISO15765 link, this service can no longer derive
/// `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE` as objective facts of
/// its own -- ISO 22900-2:2022 Annex D.2.1 Table D.4 documents both as
/// "RAW_MODE Only" TxFlag bits, meaningful ONLY as client input, so this
/// function maps the client's own two bits straight through to native
/// `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE` instead of dropping them, and
/// `apply_resolved_tx_flags` (every call site's next step) skips its own
/// override of those same two positions for a RawMode CAN/ISO15765 CLL --
/// see that function's own doc comment.
fn compute_j2534_tx_flags(
    ctrl_data: &vci_service_interface::ComPrimitiveCtrlData,
    raw_mode: bool,
    hw_protocol_id: u32,
) -> u32 {
    use vci_service_interface::com_primitive_ctrl_data::TxFlag;
    let base_protocol_id = resources::base_protocol_id(hw_protocol_id);
    // Codex review, PR #107 round 6: `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE`
    // are NOT gated identically -- `tx_header::can_addressing_tx_flags`'s
    // own pre-existing (non-RawMode) invariant already establishes why:
    // `TX_EXTENDED_ID` (11- vs. 29-bit CAN ID width) "applies regardless of
    // protocol," but `ISO15765_ADDR_TYPE` is an ISO15765-specific
    // extended-addressing indicator (SAE J2534-1 Table B.13) that "must
    // never be set on a plain raw-CAN link." A RawMode base-CAN CLL setting
    // the client-authoritative `TxFlagIso15765AddrType` bit must therefore
    // still drop it, not just SCI/Analog Inputs (the round-3 fix) -- only a
    // genuinely ISO15765 link (`resources::base_protocol_id`-normalized, so
    // FD/SW-qualified variants still count) may pass it through.
    let raw_mode_can_family =
        raw_mode && matches!(base_protocol_id, j2534_0404::CAN | j2534_0404::ISO15765);
    let raw_mode_iso15765 = raw_mode && base_protocol_id == j2534_0404::ISO15765;
    match &ctrl_data.tx_flag {
        Some(TxFlag::TxFlagBits(bits_msg)) => bits_msg.bits.iter().fold(0u32, |acc, &bit_val| {
            acc | tx_flag_bit_to_j2534(bit_val)
                | raw_mode_tx_flag_bit_to_j2534(raw_mode_can_family, raw_mode_iso15765, bit_val)
        }),
        Some(TxFlag::TxFlagRaw(raw)) => {
            let byte = |i: usize| raw.get(i).copied().unwrap_or(0);
            let mut flags = 0u32;
            if byte(2) & 0x02 != 0 {
                flags |= j2534_0404::TX_WAIT_P3_MIN_ONLY;
            }
            if byte(3) & 0x40 != 0 {
                flags |= j2534_0404::TX_ISO15765_FRAME_PAD;
            }
            // ISO 22900-2 D.2.1/Table D.4 raw byte-array equivalents of
            // `TxFlagCan29bitId`/`TxFlagIso15765AddrType` (ADR-196 Decision
            // item 2) -- see this function's own doc comment.
            if raw_mode_can_family && byte(2) & 0x01 != 0 {
                flags |= j2534_0404::TX_EXTENDED_ID;
            }
            if raw_mode_iso15765 && byte(3) & 0x80 != 0 {
                flags |= j2534_0404::ISO15765_ADDR_TYPE;
            }
            flags
        }
        None => 0,
    }
}

/// The `TxFlagBits` half of [`compute_j2534_tx_flags`]'s RawMode amendment
/// (ADR-196 Decision item 2) -- a single named bit's
/// `TxFlagCan29bitId`/`TxFlagIso15765AddrType` mapping to
/// `TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE`, `0` for every other bit or when
/// the relevant gate (`can_family`/`iso15765`, see `compute_j2534_tx_flags`'s
/// own doc comment for why they differ) is `false`. Kept separate from
/// [`tx_flag_bit_to_j2534`] rather than folded into it, so the two mapping
/// concerns (ordinary flag bits vs. RawMode-only addressing bits) stay
/// independently testable/reusable.
///
/// **`pub(super)` since ADR-199:** also reused by
/// `rpc_misc::ioctl_start_repeat_message`'s Repeat Messaging `tx_flag_bits`
/// composition (SAE J2534-2 clause 14) -- RawMode Repeat Messaging support
/// (ADR-199) gives that function a genuine RawMode concept of its own (its
/// calling CLL's `LogicalLinkState::raw_mode`), reusing this identical
/// per-bit mapping for the transmitted `repeat_msg_data` message's TxFlags.
/// ADR-214 extends the reuse a second, independent time: the mask/pattern
/// response template's own addressing basis is folded through this same
/// mapping too, but from a SEPARATE field (`setup.response_tx_flag_bits`,
/// the packed `REPEAT_MSG_SETUP` wire format's optional v2 trailing
/// section) rather than sharing `tx_flag_bits` -- see that call site's own
/// comment for the split. This superseded an earlier version of this doc
/// comment asserting that call site "must keep dropping these two bits
/// unconditionally," which ADR-199 makes false.
pub(super) fn raw_mode_tx_flag_bit_to_j2534(can_family: bool, iso15765: bool, bit_val: i32) -> u32 {
    let bit = vci_service_interface::TxFlagBit::try_from(bit_val)
        .unwrap_or(vci_service_interface::TxFlagBit::Unspecified);
    match bit {
        vci_service_interface::TxFlagBit::TxFlagCan29bitId if can_family => {
            j2534_0404::TX_EXTENDED_ID
        }
        vci_service_interface::TxFlagBit::TxFlagIso15765AddrType if iso15765 => {
            j2534_0404::ISO15765_ADDR_TYPE
        }
        _ => 0,
    }
}

/// ADR-196 Decision item 3b: the raw CAN-ID prefix width (bytes) a
/// RawMode=ON CLL's own `cop_data` carries before the actual UDS/KWP
/// payload -- `4` bytes, or `5` with an ISO15765 Address Extension byte
/// (`TX_FLAG_ISO15765_ADDR_TYPE`, client-authoritative under RawMode per
/// Decision item 2), read from `resolved_tx_flags` -- the RESOLVED native
/// J2534 TX flags this send actually used (`ResolvedSendRecvTx::tx_flags`),
/// not a re-read of `CP_...` ComParams. `0` for RawMode=OFF
/// UNCONDITIONALLY, regardless of `resolved_tx_flags` -- every one of this
/// function's call sites (`request_sid`'s capture, `SessionTimingConfig::
/// with_request`'s session-response snapshot) must resolve to exactly `0`
/// for a non-RawMode CLL, so this single shared helper is the ONE place
/// that decision is made, closing the three-independent-derivations drift
/// risk `edge-case-hunter`'s audit flagged.
///
/// `hw_protocol_id` (`LogicalLinkState::hw_protocol_id`) gates the 5-byte
/// widening to ISO15765 only, mirroring the RX-side anchor
/// (`header_footer_len`'s `j2534_0404::CAN` arm, `events.rs`) which is
/// hardcoded `data.len().min(4)` and never widens to 5 regardless of any
/// flag -- only its `ISO15765` arm's extended-addressing check does that.
/// Phase 1 allows RawMode=ON for base CAN as well as hardware ISO15765
/// (Decision item 1), and `compute_j2534_tx_flags`/
/// `raw_mode_tx_flag_bit_to_j2534` pass a RawMode client's
/// `TX_FLAG_ISO15765_ADDR_TYPE` bit through unconditionally with no
/// protocol check of their own -- so a base-CAN RawMode client that sets
/// that ISO15765-labeled bit must NOT widen this helper's prefix past 4,
/// or `request_sid`/`RcHandlingConfig`'s SID-echo anchor reads one byte
/// off from the client's actual SID, reintroducing the exact
/// misclassification bug class this Decision item exists to close
/// (`edge-case-hunter` audit finding, round 2).
///
/// Two further corrections from a follow-up `edge-case-hunter` audit
/// (round 3), both reachable within Phase 1's own RawMode allowlist:
///
/// - **FD normalization:** `hw_protocol_id` is compared via
///   `resources::base_protocol_id`, not raw equality -- a RawMode=ON
///   hardware-ISO15765 CLL's `hw_protocol_id` can be mutated post-create to
///   the `_PS`-qualified `PROTOCOL_FD_ISO15765_PS` by
///   `J2534Service::apply_fd_mode` (staged `CP_CANFDTxMaxDataLength`/
///   `CP_CANFDBaudrate`, which that function reads with no `raw_mode` check
///   of its own), the same "protocol-family decision needs
///   `base_protocol_id` normalization" pattern `resolve_send_recv_tx`
///   itself already follows (ADR-157 Plane B) and the RX-side
///   `header_footer_len` anchor already gets via
///   `events_rx_routing.rs`'s own `header_protocol` derivation.
/// - **Analog Inputs/SCI no-op:** Decision item 1's RawMode allowlist also
///   accepts Analog Inputs (`resources::is_analog_in_protocol_id`) and the
///   four SAE J2610 SCI ids as a documented no-op exception, specifically
///   because `header_footer_len`'s catch-all arm already returns `(0, 0)`
///   for them -- RawMode ON/OFF are observably identical. Falling through
///   to this function's plain-CAN `4`-byte default for these ids (as a
///   prior revision of this function did) breaks that no-op guarantee by
///   desyncing `request_sid`/`RcHandlingConfig`'s anchor from the RX side
///   by 4 bytes on a link that has no header to skip at all.
///
/// **ADR-198 Phase 2:** a K-line (ISO9141/ISO14230) arm, added once RawMode
/// extends to hardware K-line. Unlike CAN/ISO15765's fixed 4/5-byte prefix,
/// KWP's header width is data-dependent (`tx_header::kwp_header_bytes`'s TX
/// composition varies 2-4 bytes by format byte) -- so this arm reuses
/// `events::kwp_header_and_payload_len` (the identical RX-side parser,
/// `pub(super)`-exposed for this call) against `cop_data` itself, rather
/// than re-deriving a second, possibly diverging notion of "how wide is
/// this frame's header." A frame that does not parse as a well-formed KWP
/// header shape at all (`kwp_header_and_payload_len` returns `None`, or its
/// declared header+payload length exceeds `cop_data`'s actual length)
/// mirrors `header_footer_len`'s own degenerate-frame fallback exactly:
/// the whole buffer is treated as header (`cop_data.len()`), so
/// `request_sid`/`RcHandlingConfig`'s anchors land past the end of the
/// buffer and decline to classify, rather than misreading arbitrary bytes
/// as a SID -- ADR-147's own "decline rather than risk a false positive"
/// philosophy, reused rather than reinvented (documented, not asserted
/// only in a comment: see `kline_raw_mode.rs`'s malformed-header test).
///
/// **ADR-200 Phase 3:** a SAE J1850 (VPW/PWM) arm, added once RawMode
/// extends there -- J1850's native header is a fixed 3 bytes (`tx_header::
/// j1850_header_bytes`), not the CAN family's 4/5-byte shape, so it needs
/// its own arm rather than falling through to the default. SAE J1939
/// (`PROTOCOL_J1939_PS`) deliberately gets NO dedicated arm -- its RawMode
/// client contract is a plain 4-byte CAN ID, identical to the fall-through
/// default, and `cop_data` here is always the client's pre-DA-insertion
/// buffer (see the function body's own comment on this).
fn compute_tx_prefix(
    raw_mode: bool,
    hw_protocol_id: u32,
    resolved_tx_flags: u32,
    cop_data: &[u8],
) -> usize {
    if !raw_mode {
        return 0;
    }
    if resources::is_analog_in_protocol_id(hw_protocol_id)
        || matches!(
            hw_protocol_id,
            j2534_0404::SCI_A_ENGINE
                | j2534_0404::SCI_A_TRANS
                | j2534_0404::SCI_B_ENGINE
                | j2534_0404::SCI_B_TRANS
        )
    {
        return 0;
    }
    let base_protocol_id = resources::base_protocol_id(hw_protocol_id);
    if base_protocol_id == j2534_0404::ISO9141 || base_protocol_id == j2534_0404::ISO14230 {
        return match events::kwp_header_and_payload_len(cop_data) {
            Some((header, payload_len)) if header + payload_len <= cop_data.len() => header,
            _ => cop_data.len(),
        };
    }
    // ADR-200 (Phase 3): SAE J1850's native header is a fixed 3 bytes
    // (`tx_header::j1850_header_bytes`'s own composition; `events::
    // header_footer_len`'s `J1850PWM`/`J1850VPW` arms both use the identical
    // `data.len().min(3)` on the RX side) -- NOT the CAN-family's 4/5-byte
    // shape the fall-through default below would otherwise wrongly apply to
    // it now that J1850 is RawMode-admitted (ADR-200). Without this arm,
    // `request_sid`/`RcHandlingConfig`'s SID-echo anchor would land one byte
    // off from the client's actual SID for every RawMode J1850 CLL.
    if matches!(
        base_protocol_id,
        j2534_0404::J1850VPW | j2534_0404::J1850PWM
    ) {
        return cop_data.len().min(3);
    }
    if base_protocol_id == j2534_0404::ISO15765
        && resolved_tx_flags & j2534_0404::ISO15765_ADDR_TYPE != 0
    {
        5
    } else {
        4
    }
    // ADR-200 (Phase 3): SAE J1939 (`PROTOCOL_J1939_PS`) is deliberately NOT
    // given its own arm above -- it falls through to this plain `4` default,
    // which is already correct: the RawMode SAE J1939 client contract (ISO
    // 22900-2:2022 line 774/Table 80) is a 4-byte 29-bit CAN identifier plus
    // payload, the identical shape CAN's own default already covers, and
    // `cop_data` here is the client's ORIGINAL raw buffer (before
    // `tx_header::raw_j1939_tx_message`'s DA-byte insertion runs inside
    // `build_tx_message`) -- so no widening to 5 is needed or correct here,
    // unlike ISO15765's extended-addressing case just above.
}

#[cfg(test)]
mod compute_tx_prefix_tests {
    use super::*;

    /// RawMode=OFF resolves to `0` unconditionally, regardless of whatever
    /// bits happen to be set in `resolved_tx_flags` -- the single most
    /// important invariant this helper exists to guarantee (ADR-196
    /// Decision item 3b's "byte-for-byte unchanged for RawMode=OFF"
    /// requirement).
    #[test]
    fn raw_mode_off_is_always_zero_regardless_of_resolved_flags() {
        assert_eq!(compute_tx_prefix(false, j2534_0404::ISO15765, 0, &[]), 0);
        assert_eq!(
            compute_tx_prefix(
                false,
                j2534_0404::ISO15765,
                j2534_0404::ISO15765_ADDR_TYPE,
                &[]
            ),
            0
        );
        assert_eq!(
            compute_tx_prefix(false, j2534_0404::ISO15765, u32::MAX, &[]),
            0
        );
        assert_eq!(compute_tx_prefix(false, j2534_0404::CAN, u32::MAX, &[]), 0);
    }

    /// RawMode=ON without the resolved `ISO15765_ADDR_TYPE` bit is a plain
    /// 4-byte CAN-ID prefix.
    #[test]
    fn raw_mode_on_without_addr_type_bit_is_four() {
        assert_eq!(compute_tx_prefix(true, j2534_0404::ISO15765, 0, &[]), 4);
    }

    /// RawMode=ON WITH the resolved `ISO15765_ADDR_TYPE` bit widens to 5
    /// bytes (the extra Address Extension byte) -- read from the RESOLVED
    /// native TX flags, matching a client-authoritative
    /// `TX_FLAG_ISO15765_ADDR_TYPE` under RawMode (Decision item 2) -- but
    /// only for a genuinely ISO15765 CLL.
    #[test]
    fn raw_mode_on_with_addr_type_bit_is_five_for_iso15765() {
        assert_eq!(
            compute_tx_prefix(
                true,
                j2534_0404::ISO15765,
                j2534_0404::ISO15765_ADDR_TYPE,
                &[]
            ),
            5
        );
    }

    /// A base-CAN RawMode CLL never widens past 4, even if the client sets
    /// the ISO15765-labeled `TX_FLAG_ISO15765_ADDR_TYPE` bit -- mirroring
    /// `header_footer_len`'s `j2534_0404::CAN` arm, which is hardcoded
    /// `data.len().min(4)` and never widens to 5 regardless of any flag.
    /// Without this protocol gate, a base-CAN RawMode client setting that
    /// bit would desync `request_sid`'s SID-echo capture from the RX-side
    /// anchor by one byte (`edge-case-hunter` audit finding, round 2).
    #[test]
    fn raw_mode_on_with_addr_type_bit_stays_four_for_plain_can() {
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::CAN, j2534_0404::ISO15765_ADDR_TYPE, &[]),
            4
        );
    }

    /// A RawMode=ON hardware-ISO15765 CLL whose `hw_protocol_id` was
    /// substituted post-create to the CAN-FD-qualified
    /// `PROTOCOL_FD_ISO15765_PS` (`J2534Service::apply_fd_mode`, staged
    /// `CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate`, which applies with no
    /// `raw_mode` check of its own) still widens to 5 with the resolved
    /// `ISO15765_ADDR_TYPE` bit -- `resources::base_protocol_id` normalizes
    /// the `_PS`-qualified id the same way `resolve_send_recv_tx` and the
    /// RX-side `header_footer_len` anchor already do (`edge-case-hunter`
    /// audit finding, round 3).
    #[test]
    fn raw_mode_on_with_addr_type_bit_is_five_for_fd_qualified_iso15765() {
        assert_eq!(
            compute_tx_prefix(
                true,
                j2534_0404::PROTOCOL_FD_ISO15765_PS,
                j2534_0404::ISO15765_ADDR_TYPE,
                &[]
            ),
            5
        );
    }

    /// Analog Inputs and SAE J2610 SCI are Decision item 1's documented
    /// no-op RawMode exception: `header_footer_len` already returns `(0,
    /// 0)` for both on the RX side, so `compute_tx_prefix` must resolve to
    /// `0` for them too, not fall into the plain-CAN `4`-byte default
    /// (`edge-case-hunter` audit finding, round 3) -- otherwise
    /// `request_sid`/`RcHandlingConfig`'s anchor desyncs from the RX side
    /// by 4 bytes on a link that has no header to skip at all.
    #[test]
    fn raw_mode_on_is_zero_for_analog_inputs_and_sci_no_op_exception() {
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::PROTOCOL_ANALOG_IN_1, u32::MAX, &[]),
            0
        );
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::SCI_A_ENGINE, u32::MAX, &[]),
            0
        );
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::SCI_A_TRANS, u32::MAX, &[]),
            0
        );
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::SCI_B_ENGINE, u32::MAX, &[]),
            0
        );
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::SCI_B_TRANS, u32::MAX, &[]),
            0
        );
    }

    /// ADR-198 Phase 2: an unaddressed (`0x00`-shaped) KWP format byte with
    /// an embedded nonzero length -- 1-byte header, matching
    /// `kwp_header_and_payload_len`'s own unaddressed/embedded-length case.
    #[test]
    fn raw_mode_on_kline_unaddressed_embedded_length_header_is_one() {
        // format 0x03 (unaddressed, 3-byte embedded payload length) + 3
        // payload bytes.
        let cop_data = [0x03, 0x22, 0xF1, 0x90];
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::ISO14230, 0, &cop_data),
            1
        );
    }

    /// ADR-198 Phase 2: a CARB/ISO9141-2 addressed (`0x40`-shaped) KWP
    /// format byte always has a fixed 3-byte header (format, target,
    /// source), regardless of payload length.
    #[test]
    fn raw_mode_on_kline_carb_addressed_header_is_three() {
        // format 0x68 (0x40 | low bits), target 0x6A, source 0xF1, payload
        // 0x22 0xF1 0x90.
        let cop_data = [0x68, 0x6A, 0xF1, 0x22, 0xF1, 0x90];
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::ISO9141, 0, &cop_data),
            3
        );
    }

    /// ADR-198 Phase 2: an ISO14230 addressed (`0x80`/`0xC0`-shaped) format
    /// byte with a zero embedded length falls back to the 4-byte
    /// explicit-length-byte shape (format, target, source, length).
    #[test]
    fn raw_mode_on_kline_addressed_explicit_length_byte_header_is_four() {
        // format 0x80 (addressed, no embedded length), target 0x10, source
        // 0xF1, explicit length byte 0x03, then 3 payload bytes.
        let cop_data = [0x80, 0x10, 0xF1, 0x03, 0x22, 0xF1, 0x90];
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::ISO14230, 0, &cop_data),
            4
        );
    }

    /// ADR-198 Phase 2: a malformed/too-short KWP frame (declared header +
    /// payload length exceeds the buffer's actual length) falls back to
    /// treating the WHOLE buffer as header, mirroring `header_footer_len`'s
    /// own degenerate-frame fallback -- so downstream anchors
    /// (`request_sid`/`RcHandlingConfig`) land past the buffer and decline
    /// to classify rather than misreading arbitrary bytes as a SID.
    #[test]
    fn raw_mode_on_kline_malformed_frame_treats_whole_buffer_as_header() {
        // format 0x83 (unaddressed, embedded length 3) but only 1 payload
        // byte actually present -- declared header(1) + payload(3) = 4 >
        // buffer length (2).
        let cop_data = [0x83, 0x22];
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::ISO14230, 0, &cop_data),
            cop_data.len()
        );
    }

    /// ADR-198 Phase 2: an empty `cop_data` buffer (no format byte at all)
    /// also falls back to the whole-buffer-as-header default -- `0` in this
    /// degenerate case, matching `kwp_header_and_payload_len`'s own `None`
    /// return for an empty slice.
    #[test]
    fn raw_mode_on_kline_empty_buffer_is_zero() {
        assert_eq!(compute_tx_prefix(true, j2534_0404::ISO14230, 0, &[]), 0);
    }

    /// ADR-200 (Phase 3): SAE J1850's fixed 3-byte native header -- NOT the
    /// CAN family's 4/5-byte default a J1850 RawMode CLL would otherwise
    /// wrongly fall through to.
    #[test]
    fn raw_mode_on_j1850_header_is_three() {
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::J1850VPW, 0, &[0x68, 0x6A, 0xF1, 0x22]),
            3
        );
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::J1850PWM, 0, &[0x61, 0x6A, 0xF1, 0x22]),
            3
        );
    }

    /// A `cop_data` buffer shorter than J1850's fixed 3-byte header caps at
    /// the buffer's own length, mirroring `header_footer_len`'s identical
    /// `data.len().min(3)` degenerate case.
    #[test]
    fn raw_mode_on_j1850_short_buffer_caps_at_buffer_length() {
        assert_eq!(compute_tx_prefix(true, j2534_0404::J1850VPW, 0, &[0x68]), 1);
    }

    /// ADR-200 (Phase 3): SAE J1939 deliberately falls through to the plain
    /// 4-byte default -- its RawMode client contract is a 4-byte 29-bit CAN
    /// identifier, the same shape CAN's own default already covers.
    #[test]
    fn raw_mode_on_j1939_falls_through_to_the_four_byte_default() {
        assert_eq!(
            compute_tx_prefix(true, j2534_0404::PROTOCOL_J1939_PS, u32::MAX, &[]),
            4
        );
    }
}

/// Maps a single raw proto `TxFlagBit` enum value to the J2534 `TxFlags` bit
/// it names, or `0` for an unrecognized/`Unspecified` value -- the per-bit
/// mapping `compute_j2534_tx_flags`'s `TxFlagBits` arm above uses, factored
/// out (Codex review, ADR-165 PR #42 round 7, Finding 2) so
/// `rpc_misc::ioctl_start_repeat_message`'s `tx_flag_bits` composition for
/// its local `RepeatMessageSetup` struct (ADR-178) can reuse the identical
/// bit mapping instead of duplicating it. Only the two bits
/// `compute_j2534_tx_flags` itself maps are
/// covered here -- see that function's doc comment for why
/// `TxFlagCan29bitId`/`TxFlagIso15765AddrType` are deliberately excluded.
pub(super) fn tx_flag_bit_to_j2534(bit_val: i32) -> u32 {
    let bit = vci_service_interface::TxFlagBit::try_from(bit_val)
        .unwrap_or(vci_service_interface::TxFlagBit::Unspecified);
    match bit {
        vci_service_interface::TxFlagBit::TxFlagWaitP3MinOnly => j2534_0404::TX_WAIT_P3_MIN_ONLY,
        vci_service_interface::TxFlagBit::TxFlagIso15765FramePad => {
            j2534_0404::TX_ISO15765_FRAME_PAD
        }
        _ => 0,
    }
}

/// Clears `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE` from `tx_flags`
/// and replaces them with the bits `can_addressing` implies (protocol-gated,
/// per `tx_header::can_addressing_tx_flags`'s own doc comment — backlog fix,
/// Codex review PR #42 round 5 Finding K investigation), and ORs in
/// `ComParamSet::sci_tx_flags` (`TX_FLAG_SCI_MODE`/`TX_FLAG_SCI_TX_VOLTAGE`,
/// from `CP_SCITransmitMode`/`CP_SCISetProgVoltage`) — both are objective
/// facts this service already resolved, not caller preference (ADR-062).
///
/// ADR-164 Decision 2/Phase 4 (re-keyed by ADR-212 Decision item 4's own
/// class of fix -- the same follow-up class as ADR-209 Decision item 7's
/// J1708 re-key just below): also ORs in `ComParamSet::sw_can_tx_flags`
/// (`TX_FLAG_SW_CAN_HV_TX`), but ONLY when `hw_protocol_id` is a genuine SW
/// link (`resources::is_sw_family_protocol_id`) -- unlike `sci_tx_flags`,
/// `CP_SwCan_HighVoltage` is allowlisted CAN-family-wide, so a dual-wire CAN
/// link's ComParams can carry a nonzero value here that must never reach the
/// wire; gating here (the one place every TX-flags call site funnels
/// through) is what keeps the bit from bleeding onto a non-SW CAN link.
///
/// ADR-175 Decision 6/Phase 11, re-keyed by ADR-209 Decision item 7: also
/// ORs in `ComParamSet::msg_priority_tx_flags` (`TX_FLAG_MSG_PRIORITY_VALUE`),
/// but ONLY when `hw_protocol_id` is a genuine J1708 link
/// (`resources::is_j1708_family_protocol_id`) -- the same gating shape as
/// `sw_can_tx_flags` above, since `PARAM_MESSAGE_PRIORITY` is allowlisted on
/// several unrelated protocol families too (see
/// `comparam_support::is_can_param`/`is_kwp_param`/`is_j1850pwm_param`/
/// `is_j1850vpw_param`) and must not bleed onto their TxFlags. Like
/// `sw_can_tx_flags`, this is an objective per-link ComParam value this
/// service already resolved, not a caller-supplied per-message preference.
/// Called from all three of this module's TX-flags resolution sites
/// (`resolve_send_recv_tx`, `resolve_tester_present`, `resolve_init_tx_flags`)
/// with the same `hw_protocol_id` each already resolves against -- this
/// `hw_protocol_id` is the *connected link's* raw hardware protocol id, so
/// clause 17's `CP_MessagePriority` semantics apply identically whether the
/// link is `_PS`- or `_CHx`-connected. Gating on the narrower, arm-gate-only
/// `is_j1708_protocol_id` predicate would silently drop
/// `TX_FLAG_MSG_PRIORITY_VALUE` from every send on a `_CHx`-connected J1708
/// link; `is_j1708_family_protocol_id` (ADR-209) closes that gap -- this is
/// a genuinely new call-site FILE ADR-207/ADR-208 never touched (their own
/// family-wide TX-flags fixes landed in `resources.rs`/`rpc_misc.rs`/
/// `comparam_id.rs` instead). Propagates uniformly to CoptSendrecv,
/// tester-present, and periodic-frame sends alike -- the same "objective
/// fact, not caller preference" treatment `sci_tx_flags` already gets on all
/// three (ADR-164 Consequences: decided here to match that existing
/// precedent). `hw_protocol_id` is now also forwarded to
/// `can_addressing_tx_flags` for the identical reason.
/// `tp20_established_tx_id` (Codex review finding, P1, PR #97, round 18):
/// unlike SAE J1939 (below), a TP2.0 connection's device-assigned TX-ID is
/// not unconditionally 29-bit -- clause 19 permits either an 11-bit or
/// 29-bit CAN identifier, decided by the device's own address assignment
/// (this mock's own formula, `rx_id_proposal | 0x1000_0000`, always
/// produces one above `0x7FF`, but that is a mock-implementation choice,
/// not a clause 19 guarantee). `can_addressing_tx_flags(None, ..)` above
/// always returns `0` for TP2.0 (`resolve_can_addressing` is keyed on
/// `CP_CanPhysReqId`/`CP_CanFuncReqId`, which TP2.0 never configures, the
/// identical gap the J1939 branch below already documents for itself) --
/// so `TX_EXTENDED_ID` was never set for an ordinary TP2.0 `CoptSendrecv`,
/// submitting a genuinely 29-bit TX-ID as an 11-bit frame. `None` for
/// every non-TP2.0 caller (this function's other two call sites, where
/// TP2.0 never reaches this code path at all).
fn apply_resolved_tx_flags(
    tx_flags: u32,
    can_addressing: Option<tx_header::CanAddressing>,
    active: &ComParamSet,
    hw_protocol_id: u32,
    tp20_established_tx_id: Option<u32>,
    // ADR-196 Decision item 2, extended by ADR-198 Phase 2: `true` for a
    // RawMode=ON CLL -- the protocol allowlist (`rpc_link::
    // rpc_create_com_logical_link`) accepts this for a base CAN or hardware
    // ISO15765 link, or a hardware K-line (ISO9141/ISO14230) link. For
    // K-line, skipping the `can_addressing_tx_flags` derivation below is a
    // functional no-op either way: that helper is CAN-specific and already
    // returns `0` for a non-CAN-family `hw_protocol_id`, and
    // `compute_j2534_tx_flags` never sets `TX_EXTENDED_ID`/
    // `ISO15765_ADDR_TYPE` for a K-line CLL in the first place (those bits
    // are CAN/ISO15765-gated there too). `compute_j2534_tx_flags` has already mapped the client's own
    // `TX_FLAG_CAN_29BIT_ID`/`TX_FLAG_ISO15765_ADDR_TYPE` bits straight
    // through onto `tx_flags` for such a CLL -- this service built no header
    // of its own to derive "objective facts" about (ADR-062's own reasoning
    // no longer applies once RawMode hands header construction to the
    // client), so those two bits are left exactly as the client set them
    // instead of being cleared and overridden by `can_addressing_tx_flags`
    // below. RawMode=OFF (the `false` arm) is byte-for-byte the pre-ADR-196
    // behavior.
    raw_mode: bool,
) -> u32 {
    let mut flags = if raw_mode {
        tx_flags | active.sci_tx_flags()
    } else {
        (tx_flags & !(j2534_0404::TX_EXTENDED_ID | j2534_0404::ISO15765_ADDR_TYPE))
            | tx_header::can_addressing_tx_flags(can_addressing, hw_protocol_id)
            | active.sci_tx_flags()
    };
    if resources::is_sw_family_protocol_id(hw_protocol_id) {
        flags |= active.sw_can_tx_flags();
    }
    if resources::is_j1708_family_protocol_id(hw_protocol_id) {
        flags |= active.msg_priority_tx_flags();
    }
    // SAE J2534-2 clause 16.4.3 (Codex review, PR #72 round 11): a SAE J1939
    // frame's CAN identifier is always 29-bit, but `tx_header::
    // resolve_can_addressing` returns `None` for J1939 (it is keyed on
    // `CP_CanPhysReqId`/`CP_CanFuncReqId`, which J1939 never configures --
    // addressing is resolved from `CP_J1939TargetAddress`/the claim state
    // machine instead), so `can_addressing_tx_flags(None, ..)` above always
    // returns `0` and this bit was cleared, never set, for every ordinary
    // J1939 `CoptSendrecv`, the optional `CoptStartcomm` message, and
    // tester-present frame -- each submitted as an 11-bit transmission the
    // native library may reinterpret or reject. Forced here unconditionally
    // for a J1939 link, the same "objective fact the service already knows,
    // not caller preference" treatment `sci_tx_flags`/`sw_can_tx_flags`/
    // `msg_priority_tx_flags` already get above.
    if resources::is_j1939_protocol_id(hw_protocol_id) {
        flags |= j2534_0404::TX_EXTENDED_ID;
    }
    // SAE J2534-2 clause 19.3.2.2 (ADR-192/Phase 7 Stage 7c): resolved once,
    // ahead of the `tp20_established_tx_id` check just below, so that check
    // can suppress itself for a broadcast send -- see its own comment.
    // ADR-210 Decision item 9: re-keyed from the narrow `is_tp2_0_protocol_id`
    // to `is_tp2_0_family_protocol_id` -- `hw_protocol_id` here is a live
    // link's raw, un-normalized id, so a `_CHx`-connected TP2.0 link would
    // otherwise never resolve as a broadcast send.
    let tp20_is_broadcast = resources::is_tp2_0_family_protocol_id(hw_protocol_id)
        && active.tp20_broadcast_address().is_some();
    // See this function's own doc comment for `tp20_established_tx_id`
    // just above: derived from the established TX-ID's own value, not
    // forced unconditionally the way J1939's always-29-bit ID is above.
    // Codex review fix (P1, PR #101, ADR-192/Phase 7 Stage 7c): gated on
    // `!tp20_is_broadcast` -- a CLL can have BOTH an established connection
    // AND stage a broadcast for one send (ADR-192 Decision item 1,
    // broadcast is per-send, connection-independent), so
    // `tp20_established_tx_id` can be `Some` even when THIS send is a
    // broadcast. A broadcast frame's `[address] ++ payload` composition
    // (`tx_header::build_tx_message`'s `PROTOCOL_TP2_0_PS` broadcast arm)
    // carries no relationship to the established connection's own TX-ID
    // width, so forcing `TX_EXTENDED_ID` from that unrelated connection
    // state would tag the broadcast with the wrong CAN-ID-format flag.
    if !tp20_is_broadcast && tp20_established_tx_id.is_some_and(|tx_id| tx_id > 0x7FF) {
        flags |= j2534_0404::TX_EXTENDED_ID;
    }
    // A TP2.0 broadcast send ORs in `TX_FLAG_TP2_0_BROADCAST_MSG` -- the
    // same `is_tp2_0_family_protocol_id` gating shape `sw_can_tx_flags`/
    // `msg_priority_tx_flags` require of their own ComParam-derived bits
    // above, since `CP_TP20BroadcastAddress` is allowlisted for TP2.0 only
    // but this function has no other way to know the live link's protocol.
    if tp20_is_broadcast {
        flags |= j2534_0404::TX_FLAG_TP2_0_BROADCAST_MSG;
    }
    flags
}

/// Everything `CoptSendrecv` needs to actually transmit, resolved from
/// ComParams and the CLL's UniqueRespIdTable.
pub(super) struct ResolvedSendRecvTx {
    pub(super) data: Vec<u8>,
    pub(super) tx_flags: u32,
    pub(super) isotp_tx: Option<SoftIsoTpTx>,
    pub(super) can_functional: Option<bool>,
    /// ADR-180 Decision 14 (design-advisor consult, PR #72 round 12,
    /// Finding 1 Part B): the SAE J1939 source address byte this
    /// `CoptSendrecv`'s frame was actually composed with -- the identical
    /// value `build_tx_message`'s `PROTOCOL_J1939_PS` arm read via
    /// `tx_header::tester_addr` above, surfaced here so
    /// `events.rs::handle_send_recv` can re-verify at EACH transmit cycle
    /// that this still matches the CLL's CURRENT `j1939_claimed_address` --
    /// closing the enqueue-time-gate-to-dispatch-time TOCTOU (a claim can be
    /// spontaneously lost, or a multi-cycle send can still be queued from
    /// before this CLL's first claim resolved, between `start_com_primitive`
    /// accepting this COP and any given cycle actually transmitting it).
    /// `Some(source_address)` for a J1939 CLL (`resources::is_j1939_protocol_id`),
    /// `None` for every other protocol, which has no such stale-address
    /// hazard and skips the check entirely.
    pub(super) j1939_tx_source: Option<u8>,
    /// ADR-188 fix (edge-case-hunter, PR #97): the bind-time TP2.0 TX-ID
    /// this send's `data` header was actually composed with -- copied
    /// straight through from this function's own `tp20_established_tx_id`
    /// parameter, mirroring `j1939_tx_source` above. Surfaced so
    /// `events.rs::handle_send_recv`/`handle_stop_comm` can re-verify at
    /// EACH dispatch that this still matches the CLL's CURRENT
    /// `LogicalLinkState::tp20_connection` -- closing the identical
    /// enqueue-time-gate-to-dispatch-time TOCTOU `j1939_tx_source` closes
    /// for J1939 (a `CoptStopcomm`'s queued `tp20_connection = None`
    /// teardown can complete between this COP's `StartComPrimitive` bind
    /// and its actual transmit, since `CoptSendrecv` has no `comm_started`
    /// precondition gate). `None` for every non-TP2.0 protocol.
    pub(super) tp20_established_tx_id: Option<u32>,
    /// Codex review fix (P1, PR #101, ADR-192/Phase 7 Stage 7c): whether
    /// THIS send was resolved as a TP2.0 broadcast
    /// (`is_tp2_0_family_protocol_id(hw_protocol_id) &&
    /// params.tp20_broadcast_address().is_some()`, mirroring
    /// `is_broadcast_send`'s identical check in `rpc_start_com_primitive`).
    /// `tp20_established_tx_id` above is captured from the CLL's LIVE
    /// connection state independent of whether this particular send is a
    /// broadcast (a CLL can have BOTH an established connection AND stage a
    /// broadcast for one send -- ADR-192 Decision item 1, broadcast is
    /// per-send, connection-independent), so it can be `Some` even for a
    /// broadcast. Surfaced so `events.rs::handle_send_recv`/
    /// `handle_stop_comm` can skip their own TP2.0-connection-awareness
    /// dispatch-time overwrite entirely for a broadcast item -- without this
    /// flag, that overwrite unconditionally clobbers `data[0..4]` (the
    /// broadcast address byte plus 3 payload bytes) with the connection's
    /// live TX-ID whenever the connection hasn't drifted, corrupting every
    /// single-shot broadcast issued on a CLL that also has an established
    /// connection.
    pub(super) tp20_is_broadcast: bool,
}

/// SAE J2534-2 clause 19.3.2.2 (ADR-192/Phase 7 Stage 7c, Fix 4 -- Codex
/// review, P2, PR #101): rejects an out-of-range, nonzero raw
/// `CP_TP20BroadcastAddress` value. Reads the RAW staged value from `params`
/// (not through `ComParamSet::tp20_broadcast_address`, which silently folds
/// an out-of-range value to `None`/"no broadcast" by design -- see that
/// accessor's own doc comment; that fallback exists for the accessor's own
/// panic-safety, not to double as this validation). `0` (unset) is fine --
/// only a nonzero value outside `0xF0..=0xFF` is rejected.
///
/// Shared by every `resolve_send_recv_tx` call site in this file that can
/// reach a TP2.0 link: `CoptSendrecv`'s own send, and the optional-message
/// paths of `CoptStartcomm`/`CoptStopcomm` -- without this check at all
/// three, a garbage nonzero `CP_TP20BroadcastAddress` staged alongside a
/// nonempty `CoptStartcomm`/`CoptStopcomm` message silently resolved and
/// sent as an ordinary connection-bound frame instead of being rejected the
/// way an equivalent `CoptSendrecv` already is. Callers MUST gate this
/// behind `resources::is_tp2_0_family_protocol_id(hw_protocol_id)`
/// themselves (ADR-210 Decision item 9: widened from the narrow
/// `is_tp2_0_protocol_id` so a `_CHx`-connected TP2.0 link's own optional
/// message/send is validated too), the same shape `tp20_broadcast_address`
/// itself requires of its own callers.
fn validate_tp20_broadcast_address_range(params: &ComParamSet) -> Result<(), String> {
    let raw_broadcast_address = params
        .unum32
        .get(&PARAM_TP20_BROADCAST_ADDRESS)
        .copied()
        .unwrap_or(0);
    if raw_broadcast_address != 0 && !(0xF0..=0xFF).contains(&raw_broadcast_address) {
        return Err(format!(
            "CP_TP20BroadcastAddress must be 0 (no broadcast) or in 0xF0-0xFF, got \
             {raw_broadcast_address:#x}"
        ));
    }
    Ok(())
}

/// SAE J2534-1's own per-protocol composed-message (header + payload) TX
/// size range (Figure 42) -- factored out of `resolve_send_recv_tx`'s own
/// `size_range` computation (ADR-215) so `resolve_tester_present` can enforce
/// the identical constraint against its own composed tester-present message,
/// instead of a second, independently-drifting copy of this table.
///
/// - `fd_base_family` selects the FD-specific `FD_CAN_PS`
///   (`fd_can_tx_message_size_range`, ADR-159) / `FD_ISO15765_PS`
///   (`protocol::fd_iso15765_tx_message_size_range`, SAE J2534-2 Table 98)
///   ranges instead of `hw_protocol`'s Classic table row; `None` for a
///   non-FD link.
/// - `kline_manual_checksum_iso14230` widens the ISO14230 Classic ceiling by
///   one byte for SAE J2534-1 §8.3's "ISO14230 (Manual Checksum)" row
///   (ADR-198 Phase 2). `resolve_tester_present` always passes `false`: its
///   `checksum_mode` is not threaded into that function at all, and the only
///   wire-visible effect the flag has (this one-byte widening) is
///   unreachable once `CP_TesterPresentMessage`'s own `SetComParam`-time
///   12-byte ParamMaxLen cap (`rpc_link::rpc_set_com_param`) already bounds
///   every tester-present payload well under either ceiling.
/// - `tp20_is_broadcast` narrows to TP2.0's own broadcast-burst floor/ceiling
///   (SAE J2534-2 clause 19.3.2.2/19.3.2.3). `resolve_tester_present` always
///   passes `false`: TesterPresent is unreachable for TP2.0
///   (`comparam_support::is_tp20_param`'s exclusion of
///   `CP_TesterPresentHandling`).
fn sae_tx_size_range(
    hw_protocol: ChannelProtocol,
    extended_addressing: bool,
    fd_base_family: Option<u32>,
    params: &ComParamSet,
    kline_manual_checksum_iso14230: bool,
    tp20_is_broadcast: bool,
) -> RangeInclusive<usize> {
    if tp20_is_broadcast {
        3..=8
    } else {
        match fd_base_family {
            Some(j2534_0404::CAN) => fd_can_tx_message_size_range(params),
            Some(j2534_0404::ISO15765) => {
                protocol::fd_iso15765_tx_message_size_range(extended_addressing)
            }
            _ => {
                let base_range = hw_protocol.tx_message_size_range(extended_addressing);
                if kline_manual_checksum_iso14230 {
                    *base_range.start()..=(*base_range.end() + 1)
                } else {
                    base_range
                }
            }
        }
    }
}

/// Resolves addressing, the full header-prefixed message, TxFlags, and (for
/// software ISO-TP) framing options for a `CoptSendrecv`, from `params` and
/// `entries` — and validates the result (SAE J2534-1 TX size range ADR-049,
/// ISO15765-2 functional-addressing Single Frame limit ADR-055, the
/// underlying `PassThruMessage` construction itself). Every check here
/// depends on `params` (Active or Working) — the one CoptSendrecv-time check
/// that does not (`cop_data` must be non-empty in software ISO-TP mode) is
/// validated eagerly in `rpc_start_com_primitive` alongside this, not here.
///
/// Called from exactly one place: `rpc_start_com_primitive`, synchronously,
/// against the `ParamBinding` this call binds at `StartComPrimitive` call
/// time -- `binding.resolved()` (the call-time Working snapshot when
/// `temp_param_update` is set, else the call-time Active snapshot) (ADR-067,
/// reverting ADR-064's deferral of this call to the poll task's first cycle).
/// A resolution failure here is reported as a synchronous `StartComPrimitive`
/// `INVALID_ARGUMENT`, as it was before ADR-064 -- since resolution now binds
/// at the call itself rather than racing a queued `CoptUpdateparam`/
/// `CoptRestoreParam`, there is no reason to defer the validation to
/// execution time.
///
/// `tp20_established_tx_id` (Codex review PR #97 fix): threaded straight
/// through to `tx_header::build_tx_message` -- see that function's own doc
/// comment. The caller resolves this from the CLL's live
/// `LogicalLinkState::tp20_connection`, in the same critical section it
/// already captures `bound_comparams`/`connect_generation` from (ADR-086
/// discipline), not from `params`.
#[allow(clippy::too_many_arguments)]
pub(super) fn resolve_send_recv_tx(
    protocol: ChannelProtocol,
    hw_protocol_id: u32,
    params: &ComParamSet,
    entries: &[EcuUniqueRespEntry],
    cop_data: &[u8],
    software_isotp: bool,
    base_tx_flags: u32,
    tp20_established_tx_id: Option<u32>,
    // ADR-196 Decision items 1/2, extended by ADR-198 Phase 2: the calling
    // CLL's `LogicalLinkState::raw_mode` -- the protocol allowlist
    // (`rpc_create_com_logical_link`) accepts this `true` for a base CAN or
    // hardware ISO15765 `protocol`/`hw_protocol_id` (never alongside
    // `software_isotp`), for a hardware K-line (ISO9141/ISO14230) link, and
    // also for Analog Inputs/SCI as a documented no-op -- so `raw_mode`
    // alone does not imply CAN/ISO15765 (see `compute_j2534_tx_flags`'s own
    // doc comment for the protocol-gating this implies for its two
    // CAN-specific TxFlags bits). Threaded straight through to
    // `build_tx_message` (skips header construction entirely) and
    // `apply_resolved_tx_flags` (makes `TX_FLAG_CAN_29BIT_ID`/
    // `TX_FLAG_ISO15765_ADDR_TYPE` client-authoritative instead of
    // service-derived).
    raw_mode: bool,
    // ADR-198 Phase 2: the calling CLL's `LogicalLinkState::checksum_mode`
    // -- meaningful only alongside `raw_mode` on a K-line (ISO9141/
    // ISO14230) link, where it decides whether the SAE J2534-1 §8.3
    // "Manual Checksum" widened message-size range applies (see the
    // `size_range` computation below). Ignored for every other protocol.
    checksum_mode: bool,
) -> Result<ResolvedSendRecvTx, String> {
    // The `ChannelProtocol` view of the link's actual connected hardware
    // (`hw_protocol_id` is always a native J2534 ID, so this is exactly
    // `ChannelProtocol::from_raw`) -- used below for checks that must follow
    // the physical channel rather than service-level identity: header
    // construction and the SAE J2534-1 TX size range. `hw_protocol_id`
    // diverges from `protocol.j2534_protocol_id()` after a PWM win on the
    // SAE_J1850 auto-detect bus (ADR-070) and on a
    // SAE_J2610_on_SAE_J2610_SCI row's `hw_protocol_override` (pin-typing
    // amendment) -- e.g. a PWM-detected link must be validated against
    // J1850PWM's tighter TX size range (3..=10 bytes), not J1850VPW's
    // (1..=4128), or it would accept oversized messages the fixed J1850PWM
    // resource rejects.
    //
    // Software-ISO-TP mode (ADR-046) is the one divergence this must NOT
    // follow: there, `hw_protocol_id` is the raw CAN channel underneath, but
    // `full_message` below is the *logical* pre-segmentation ISO15765
    // message (up to ISO15765's 4095-byte payload), not literally what
    // reaches `PassThruWriteMsgs` -- the poll task segments it into
    // individually-sized raw CAN frames later. Falling back to `protocol`
    // (service-level, still ISO15765-family) keeps validating against
    // ISO15765's size range in that mode, matching pre-existing behavior;
    // `build_tx_message` does not actually care which of CAN/ISO15765 it
    // gets since both hit the identical header-construction branch, so this
    // fallback costs nothing there either.
    let hw_protocol = if software_isotp {
        protocol
    } else {
        // ADR-157 Plane B: normalized -- header format/TX size range are
        // protocol-family decisions, so a `_PS` id must resolve to its base
        // `ChannelProtocol` here. `hw_protocol_id` itself (used below for
        // the real `PassThruMessage::new` call, Plane A) stays raw.
        ChannelProtocol::from_raw(resources::base_protocol_id(hw_protocol_id))
    };

    // ADR-050: cop_data is payload-only. Build the full PassThruMessage.Data
    // (ID/header prefix + payload) from ComParams and the CLL's
    // UniqueRespIdTable (only its first entry is consulted).
    let can_addressing =
        tx_header::resolve_can_addressing(tx_header::AddrModeSource::Request, params, entries);
    let tx_addressing = can_addressing.map_or(isotp::Addressing::Normal, |a| a.tx_addressing);
    // Codex review, PR #107 round 5: computed once, here, and reused both
    // for `can_functional` below and the functional Single Frame check
    // further down -- a RawMode client has no reason to configure
    // `CP_CanFuncReqId` (Decision item 2 makes it redundant), so
    // `can_addressing` resolves to `None` for a supported RawMode
    // configuration even when `CP_RequestAddrMode` genuinely requested
    // functional addressing. Reading `CP_RequestAddrMode` directly avoids
    // that dependency; RawMode=OFF is unchanged (`can_addressing` is always
    // resolved by this point for a send that goes on to succeed, since
    // `build_tx_message`'s own header construction requires the same
    // `CP_CanFuncReqId`).
    let is_functional_request = if raw_mode {
        tx_header::use_functional_addressing(tx_header::AddrModeSource::Request, params)
    } else {
        can_addressing.is_some_and(|a| a.functional)
    };
    // CP_P3Func/P3Phys gap enforcement (ADR-060) is a CAN-family concept
    // only -- KWP's identical param names resolve to the native P3_MIN
    // hardware timer instead (ADR-056). `is_can_family()` gives the same
    // answer for `protocol` and `hw_protocol` for every divergence that
    // exists today (CAN/ISO15765 are both CAN-family; J1850/SCI are
    // neither), so this deliberately keeps using the service-level
    // `protocol` -- no functional difference, and it reads more naturally
    // alongside the ISO15765-identity check below, which must not follow
    // `hw_protocol`.
    //
    // Codex review, PR #107 round 5: derived from `is_functional_request`
    // (not `can_addressing.map(|a| a.functional)` directly) so a RawMode
    // send with no `CP_CanFuncReqId` still reports its true functional
    // intent here -- `wait_for_p3_gap`'s `CP_P3Func` enforcement and the
    // functional TX bucket update both key off this field, and previously
    // silently saw `None` (treated as non-functional) for exactly this
    // supported RawMode configuration.
    let can_functional = protocol.is_can_family().then_some(is_functional_request);
    // Codex review, PR #107 round 6: computed once, here (moved up from the
    // functional-only Single Frame check further down, which still reuses
    // it), since the general TX size-range check immediately below ALSO
    // needs it -- `base_tx_flags` already carries the client-authoritative
    // resolved `ISO15765_ADDR_TYPE` bit at this point for a RawMode CLL, and
    // `apply_resolved_tx_flags` (which runs later in this function)
    // deliberately skips its own override of that bit for RawMode, so no
    // extra resolution step is needed.
    let tx_prefix = compute_tx_prefix(raw_mode, hw_protocol_id, base_tx_flags, cop_data);
    let mut full_message = tx_header::build_tx_message(
        hw_protocol,
        tx_header::AddrModeSource::Request,
        params,
        entries,
        cop_data,
        software_isotp,
        tp20_established_tx_id,
        raw_mode,
    )?;

    // SAE J2534-1 (PassThruMessage.Data length by protocol): reject the
    // constructed message outside the applicable Min/Max Tx range. In
    // software ISO-TP mode `full_message` is the *logical*
    // [4-byte CAN ID][payload] buffer the poll task segments into raw CAN
    // frames -- it never carries an AE byte regardless of addressing (see
    // tx_header::can_header_bytes), so the Normal range always applies to
    // it; the real per-frame capacity reduction under extended addressing
    // is enforced separately by
    // isotp::Addressing::{max_sf_payload,ff_payload_len,cf_payload_len}.
    //
    // ADR-158 correction (Codex review PR #30 round 4) / ADR-159 (Phase 3b):
    // an FD-connected link (`software_isotp` is already rejected in
    // combination with FD at connect time, `fd_can.rs`'s
    // `fd_can_with_software_isotp_is_rejected` -- the `!software_isotp`
    // guard here is belt-and-braces, not load bearing) validates against its
    // own FD size range instead of `hw_protocol`'s table row, which
    // `base_protocol_id` has already normalized down to the base family's
    // fixed Classic range. The two FD families split further here (ADR-159):
    // `FD_CAN_PS` (this service builds every raw CAN FD frame itself) tracks
    // the live staged `CP_CANFDTxMaxDataLength`; `FD_ISO15765_PS` (the
    // native adapter runs the real ISO15765 state machine) uses Table 98's
    // fixed constant range instead.
    let fd_link = !software_isotp && resources::is_fd_protocol_id(hw_protocol_id);
    let fd_base_family = fd_link.then(|| resources::base_protocol_id(hw_protocol_id));
    // Codex review, PR #107 round 6: under RawMode, derived from
    // `tx_prefix == 5` (the client-authoritative resolved `ISO15765_ADDR_
    // TYPE` bit, via `compute_tx_prefix`) instead of the ComParam-derived
    // `tx_addressing` -- a RawMode client has no reason to configure
    // `CP_Can*Format` (Decision item 2 makes it redundant), so
    // `tx_addressing` silently defaults to `Normal` even when the client's
    // own request genuinely is extended-addressed. Left uncorrected, a
    // 4-byte CAN-ID-only buffer (missing its required AE byte) would pass
    // the Normal range's `4..=4099` floor instead of being rejected against
    // the Extended range's `5..=4100` floor -- broader than the already-
    // documented ADR-196 residual (which only covers the 4100-byte upper
    // boundary), so this closes it rather than leaving it as a residual.
    // `software_isotp` is unreachable in combination with `raw_mode`
    // (Decision item 1 rejects it at `CreateComLogicalLink`), so the
    // RawMode branch does not need that guard.
    let extended_addressing = if raw_mode {
        tx_prefix == 5
    } else {
        !software_isotp && matches!(tx_addressing, isotp::Addressing::Extended(_))
    };
    // Codex review fix (P1, PR #101, ADR-192/Phase 7 Stage 7c): whether THIS
    // send resolves as a TP2.0 broadcast -- the same two-part check
    // `rpc_start_com_primitive`'s own `is_broadcast_send` uses (`is_tp20_link
    // && binding.resolved().tp20_broadcast_address().is_some()`), computed
    // here once and reused both for the size-range branch immediately below
    // and for `ResolvedSendRecvTx::tp20_is_broadcast` at this function's own
    // return, so `events.rs`'s dispatch-time TP2.0-connection-awareness
    // overwrite can tell a broadcast item apart from an ordinary
    // connection-bound one (see that field's own doc comment).
    // ADR-210 Decision item 9: re-keyed from the narrow `is_tp2_0_protocol_id`
    // to `is_tp2_0_family_protocol_id`, mirroring `apply_resolved_tx_flags`'s
    // own separate fix above -- a `_CHx`-connected TP2.0 link's send would
    // otherwise never resolve as a broadcast here either.
    let tp20_is_broadcast = resources::is_tp2_0_family_protocol_id(hw_protocol_id)
        && params.tp20_broadcast_address().is_some();
    // SAE J2534-2 clause 19.3.2.2/19.3.2.3 (ADR-192/Phase 7 Stage 7c,
    // corrected by this stage's own edge-case-hunter follow-up): a broadcast
    // send's composed message (`[address] ++ payload`, `build_tx_message`'s
    // own broadcast branch) is never 4 bytes at minimum the way an ordinary
    // connection-bound TP2.0 send's 4-byte established-TX-ID prefix is --
    // `ChannelProtocol::TP2_0_PS`'s generic `4..=4096` row below reflects
    // only the connection-bound shape (SAE J2534-1's own general per-
    // protocol table has no broadcast-specific row). The broadcast's own
    // single physical CAN frame carries the 1-byte address plus up to
    // Classic CAN's 7-byte data-field remainder, so `8` is the correct
    // ceiling -- but the floor is `3`, not `1`: clause 19.3.2.2/19.3.2.3's
    // burst behavior alternates the LAST TWO BYTES OF THE WHOLE MESSAGE
    // between `0xAA`/`0x55` on each of the 5 transmissions
    // (`j2534-0404-mock`'s own `push_tp20_broadcast_burst` implements this
    // exactly), never just the payload -- at a composed length of 1 (empty
    // payload) or 2 (one payload byte), that alternation touches index 0,
    // the address byte itself, corrupting/changing the broadcast group's
    // address mid-burst. Only at length >= 3 (1 address byte + >= 2 payload
    // bytes) does the alternating pair fall entirely within the payload,
    // never touching `Data[0]`. Checked the same priority-branch shape the
    // FD/ISO15765 special cases above use.
    // ADR-198 Phase 2: SAE J2534-1 §8.3 Figure 42's "ISO14230 (Manual
    // Checksum)" row (Min/Max Tx 1..=260, vs. the interface-managed row's
    // 1..=259) applies when the effective native `ISO9141_NO_CHECKSUM`
    // connect flag is set for this send (`raw_mode && !checksum_mode`) on
    // an ISO14230 link -- verified against the spec's own Figure 42, which
    // gives ISO14230 (Manual Checksum) a distinct, one-byte-wider row but
    // gives ISO9141 no such distinct row at all: ISO9141's plain row
    // (1..=4128) already comfortably fits a manually-included checksum
    // byte, so only ISO14230 needs this +1 adjustment.
    let kline_manual_checksum_iso14230 =
        raw_mode && !checksum_mode && hw_protocol == ChannelProtocol::ISO14230;
    // ADR-215: factored into the shared `sae_tx_size_range` helper (above),
    // also called by `resolve_tester_present`.
    let size_range = sae_tx_size_range(
        hw_protocol,
        extended_addressing,
        fd_base_family,
        params,
        kline_manual_checksum_iso14230,
        tp20_is_broadcast,
    );
    if !size_range.contains(&full_message.len()) {
        return Err(format!(
            "cop_data length {} produces a {}-byte J2534 message, outside the valid \
             TX message size range ({}..={} bytes){}",
            cop_data.len(),
            full_message.len(),
            size_range.start(),
            size_range.end(),
            match fd_base_family {
                Some(j2534_0404::CAN) =>
                    " for this link's staged CP_CANFDTxMaxDataLength".to_string(),
                Some(j2534_0404::ISO15765) => {
                    " for this FD_ISO15765_PS link's SAE J2534-2 Table 98 message size range"
                        .to_string()
                }
                _ => " for this protocol".to_string(),
            },
        ));
    }

    // ISO 15765-2 requires a functionally addressed (broadcast) request to
    // fit in a single frame -- with no specific target, there is no way to
    // negotiate FlowControl for a multi-frame exchange with whichever ECU
    // happens to answer first (ADR-055). Reject rather than silently
    // segmenting. Deliberately keys off `protocol` (service-level identity),
    // not `hw_protocol`: this is an ISO15765-2 transport-layer rule that
    // must still apply in software-ISO-TP mode, where `hw_protocol_id` is
    // the raw CAN channel underneath, not ISO15765 (ADR-046) -- using
    // `hw_protocol` here would silently disable this check for every
    // software-ISO-TP CLL. `is_functional_request` (computed once, above,
    // alongside `can_functional`) already accounts for RawMode's
    // `CP_CanFuncReqId`-optional configuration (Codex review, PR #107 round
    // 4) -- see its own doc comment.
    if protocol.j2534_protocol_id() == j2534_0404::ISO15765 && is_functional_request {
        // ADR-169: a native FD_ISO15765_PS link runs the real ISO15765 state
        // machine with genuine CAN FD frame capacity, so its Single Frame
        // limit tracks the link's staged CP_CANFDTxMaxDataLength instead of
        // the fixed Classic-CAN limit -- Classic ISO15765, FD_CAN_PS, and
        // software-ISO-TP links (rejected in combination with FD at connect
        // time, ADR-158) all keep the unchanged Classic limit.
        let is_fd_iso15765 = fd_base_family == Some(j2534_0404::ISO15765);
        // Codex review, PR #107 round 2: under RawMode `cop_data` is the
        // client's own literal [CAN ID (+ AE)][payload] frame (ADR-196
        // Decision item 2), not the payload-only buffer this Single Frame
        // limit is defined against (ISO 15765-2's SF payload capacity) --
        // subtract the same `compute_tx_prefix` width every other RawMode
        // anchor in this file already uses (ADR-196 Decision item 3b) before
        // comparing, so a genuinely in-range extended-addressed RawMode
        // request isn't rejected for carrying its own required prefix.
        // `tx_prefix` is computed once, above (alongside `extended_addressing`),
        // and reused here.
        // Codex review, PR #107 round 3: `max_sf_payload` must use the SAME
        // addressing decision `tx_prefix`/`extended_addressing` (computed
        // once, above) just used, not the ComParam-derived `tx_addressing`
        // -- under RawMode there is no reason for a client to have
        // configured `CP_Can*Format` at all (Decision item 2's whole
        // point), so `tx_addressing` silently defaults to `Normal`
        // (`Addressing::from_format`'s own `None` fallback) even when
        // `extended_addressing` says the client's raw request genuinely is
        // extended-addressed -- comparing a 7-byte Normal limit against a
        // payload that already excluded a real AE byte would admit a 7-byte
        // payload where only 6 fit. Only the AE byte's PRESENCE matters to
        // `max_sf_payload`/`fd_max_sf_payload` (`Addressing::ae_len`), not
        // its value, so `Extended(0)`'s placeholder byte is inert here.
        let functional_addressing = if raw_mode {
            if extended_addressing {
                isotp::Addressing::Extended(0)
            } else {
                isotp::Addressing::Normal
            }
        } else {
            tx_addressing
        };
        let max_sf_payload = if is_fd_iso15765 {
            functional_addressing.fd_max_sf_payload(effective_fd_tx_dl(params))
        } else {
            functional_addressing.max_sf_payload()
        };
        let functional_payload_len = cop_data.len().saturating_sub(tx_prefix);
        if functional_payload_len > max_sf_payload {
            return Err(format!(
                "cop_data length {} ({}-byte payload after the {tx_prefix}-byte RawMode prefix) \
                 exceeds the {max_sf_payload}-byte Single Frame limit for functional \
                 (CP_RequestAddrMode=2) ISO15765 addressing -- ISO 15765-2 requires \
                 functionally addressed requests to fit in a single frame{}",
                cop_data.len(),
                functional_payload_len,
                if is_fd_iso15765 {
                    " (see this link's staged CP_CANFDTxMaxDataLength)"
                } else {
                    ""
                },
            ));
        }
    }

    // Software ISO-TP mode (ADR-046): the poll task segments the payload
    // into raw CAN frames itself. Padding is performed in software, so
    // TX_ISO15765_FRAME_PAD must not reach the raw CAN channel (it is only
    // valid on ISO15765 channels). FlowControl frames are expected from the
    // CP_CanRespUSDTId paired with the first UniqueRespIdTable entry's
    // CP_CanPhysReqId (ADR-050); without a pair, any FlowControl frame is
    // accepted and both addressing modes default to Normal (extended
    // addressing, ADR-046 addendum).
    let mut hw_tx_flags = base_tx_flags;
    let isotp_tx = if software_isotp {
        hw_tx_flags &= !j2534_0404::TX_ISO15765_FRAME_PAD;
        // `cop_data` non-emptiness is validated eagerly in
        // rpc_start_com_primitive (ADR-064) -- it is ComParam-independent
        // (this buffer is always exactly `[4-byte CAN ID][payload]` in
        // software ISO-TP mode, per tx_header::can_header_bytes), so there
        // is nothing here that a queued CoptRestoreParam/CoptUpdateparam
        // could invalidate.
        let (fc_can_id, fc_rx_addressing) = can_addressing
            .map_or((None, isotp::Addressing::Normal), |a| {
                (a.fc_can_id, a.fc_rx_addressing)
            });
        Some(SoftIsoTpTx {
            framing: params.isotp_framing(base_tx_flags),
            tx_addressing,
            fc_can_id,
            fc_rx_addressing,
            n_bs_timeout_ms: params.isotp_n_bs_timeout_ms(),
        })
    } else {
        None
    };
    // TX_FLAG_CAN_29BIT_ID/ISO15765_ADDR_TYPE/SCI_MODE/SCI_TX_VOLTAGE describe
    // objective facts this service already resolved from ComParams, not
    // caller preference -- authoritative over whatever the client requested
    // for these four positions (ADR-062).
    hw_tx_flags = apply_resolved_tx_flags(
        hw_tx_flags,
        can_addressing,
        params,
        hw_protocol_id,
        tp20_established_tx_id,
        raw_mode,
    );

    // ADR-158 correction (Codex review PR #30 round 4) / ADR-159 (Phase 3b):
    // an FD-connected link's message needs two more objective facts this
    // service already resolved, same rationale as the four flags just above
    // -- TX_FD_CAN_FORMAT must be set on every FD-link TX, both families
    // (SAE J2534-2 21.4.4/Tables 99-100: a conformant module rejects any
    // message over the Classic range whose FD_CAN_FORMAT flag is unset, so
    // the size-range widening above is incomplete without this), and
    // TX_FD_CAN_BRS iff the connected link staged a nonzero
    // `CP_CANFDBaudrate` (a zero value reuses the arbitration rate for the
    // data phase -- no rate switch, so no BRS). **Padding is `FD_CAN_PS`
    // only** (ADR-159): the data portion is padded up to the smallest
    // DLC-encoded length SAE J2534-2 Table 91 accepts on the wire
    // (`CP_CanFillerByte`, ISO 22900-2's `CP_CANFDTxMaxDataLength` NOTE 5)
    // -- the size-range check above already confirmed the unpadded length
    // fits the link's staged `CP_CANFDTxMaxDataLength`, so padding never
    // grows the message past it. An `FD_ISO15765_PS` link runs the real
    // ISO15765 state machine on the native adapter, which pads/segments its
    // own frames natively (driven by the client's `TX_ISO15765_FRAME_PAD`
    // flag and the forwarded `CONFIG_ISO15765_PAD_VALUE`) -- this service
    // must NOT also pad here, or it would double-pad/corrupt the
    // already-assembled logical message the adapter expects to segment
    // itself.
    if fd_link {
        hw_tx_flags |= j2534_0404::TX_FD_CAN_FORMAT;
        if params
            .unum32
            .get(&PARAM_CANFD_BAUDRATE)
            .copied()
            .unwrap_or(0)
            != 0
        {
            hw_tx_flags |= j2534_0404::TX_FD_CAN_BRS;
        }
        if fd_base_family == Some(j2534_0404::CAN) {
            let data_len = full_message.len() - 4;
            if data_len > 8 {
                let padded_len = 4 + fd_can_padded_data_len(data_len);
                let filler = params
                    .unum32
                    .get(&PARAM_CAN_FILLER_BYTE)
                    .copied()
                    .unwrap_or(0) as u8;
                full_message.resize(padded_len, filler);
            }
        }
    }

    // Validate message data so the caller gets an immediate error instead of
    // a silent failure in the poll task (for the eager/Active call site;
    // for the deferred/Working call site this is still the last chance to
    // catch a malformed message before it would otherwise reach the adapter).
    j2534_0404::PassThruMessage::new(hw_protocol_id, 0, hw_tx_flags, 0, 0, &full_message)
        .map_err(|err| format!("invalid cop_data: {err}"))?;

    // ADR-180 Decision 14 (widened by Decision 18, design-advisor consult,
    // PR #72 round 15): surface the SAME resolved J1939 source-address byte
    // `build_tx_message`'s `PROTOCOL_J1939_PS` arm just composed the header
    // with (`tx_header::tester_addr(params)`), so `events.rs::handle_send_
    // recv` can re-verify it against this CLL's CURRENT claimed address at
    // each transmit cycle. `None` for every other protocol -- no such
    // hazard, no check to run.
    //
    // Computed for EVERY J1939 CLL now (`is_j1939_protocol_id` alone), not
    // gated on `j1939_claim_requested(params...)` the way Decision 14
    // originally read it: applicability -- whether a mismatch against
    // `link.j1939_claimed_address` should actually CANCEL the send -- is
    // decided at transmit time by the live, unspoofable `LogicalLinkState::
    // j1939_negotiation_posture` field instead (`handle_send_recv`'s
    // per-cycle check, below), not by re-deriving "negotiation requested"
    // from a possibly-Working-only per-call `params` snapshot a
    // `temp_param_update = 1` call could stage independently of this CLL's
    // real structural posture (Decision 18's whole point).
    //
    // ROUND-12 REGRESSION WARNING: an earlier revision tried gating this
    // computation on `is_j1939_protocol_id` alone with NO negotiation-
    // requested condition anywhere, which spuriously cancelled every send on
    // a genuinely non-negotiated CLL (`unclaimed_source_address_over_8_
    // bytes_fails_the_send` caught it) -- that CLL's `j1939_claimed_address`
    // is permanently `None` by design (client-managed `NODE_ADDRESS`
    // instead, ADR-180 Decision 12's own `CoptUpdateparam` guard draws the
    // same distinction), so an unconditional `Some != None` compare at
    // transmit time cancelled EVERY one of its sends. THIS revision avoids
    // that regression differently: `j1939_negotiation_posture` stays
    // `J1939NegotiationPosture::Undecided` (never `Engaged`) for a CLL that
    // never requested negotiation at its own `CoptStartcomm` (see that
    // field's own doc comment), so the transmit-time cancellation check
    // below never fires for it even though `j1939_tx_source` is now always
    // `Some` here.
    let j1939_tx_source =
        resources::is_j1939_protocol_id(hw_protocol_id).then(|| tx_header::tester_addr(params));

    Ok(ResolvedSendRecvTx {
        data: full_message,
        tx_flags: hw_tx_flags,
        isotp_tx,
        can_functional,
        j1939_tx_source,
        tp20_established_tx_id,
        tp20_is_broadcast,
    })
}

/// Codex review regression (P1, PR #97, round 18): `apply_resolved_tx_flags`
/// never set `TX_EXTENDED_ID` for a TP2.0 send, since `can_addressing_tx_flags`
/// always returns `0` for TP2.0 (no `CP_CanPhysReqId`/`CP_CanFuncReqId`
/// concept) and, unlike SAE J1939, TP2.0 had no dedicated force-set branch at
/// all before this fix.
#[cfg(test)]
mod resolve_send_recv_tx_tp20_extended_id_tests {
    use super::*;

    fn resolve(tp20_established_tx_id: Option<u32>) -> ResolvedSendRecvTx {
        resolve_send_recv_tx(
            ChannelProtocol::TP2_0_PS,
            j2534_0404::PROTOCOL_TP2_0_PS,
            &ComParamSet::default(),
            &[],
            &[0x10, 0x03],
            false,
            0,
            tp20_established_tx_id,
            false,
            false,
        )
        .unwrap()
    }

    #[test]
    fn tx_extended_id_set_when_established_tx_id_exceeds_11_bit_range() {
        let resolved = resolve(Some(0x1000_0321));
        assert_eq!(
            resolved.tx_flags & j2534_0404::TX_EXTENDED_ID,
            j2534_0404::TX_EXTENDED_ID,
            "a TX-ID above 0x7FF cannot be represented as an 11-bit CAN ID"
        );
    }

    #[test]
    fn tx_extended_id_unset_when_established_tx_id_fits_11_bit_range() {
        let resolved = resolve(Some(0x0000_0321));
        assert_eq!(
            resolved.tx_flags & j2534_0404::TX_EXTENDED_ID,
            0,
            "a TX-ID within the 11-bit range must not be flagged extended"
        );
    }

    /// ADR-192/Phase 7 Stage 7c, corrected by this stage's own
    /// edge-case-hunter follow-up (Fix 4): a composed broadcast message
    /// below the broadcast-specific `3`-byte floor (1 address byte + at
    /// least 2 payload bytes) must be rejected -- at length 1 (this test's
    /// empty payload) or 2, clause 19.3.2.2/19.3.2.3's alternating-last-two-
    /// bytes burst simulation would corrupt the address byte itself
    /// (`Data[0]`), not just the payload (`j2534-0404-mock`'s own
    /// `push_tp20_broadcast_burst` implements the alternation this way).
    /// Confirms the rejection is specifically about the broadcast-specific
    /// `3..=8` range, not the ordinary connection-bound `4..=4096` range
    /// (`ChannelProtocol::TP2_0_PS.tx_message_size_range`) -- an empty
    /// payload would be rejected by EITHER range, so the assertion checks
    /// the error message cites the broadcast-specific bounds.
    #[test]
    fn broadcast_message_below_the_three_byte_address_safety_floor_is_rejected() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, 0xF3);
        let err = match resolve_send_recv_tx(
            ChannelProtocol::TP2_0_PS,
            j2534_0404::PROTOCOL_TP2_0_PS,
            &params,
            &[],
            &[], // empty payload -- composed message is just [0xF3], 1 byte
            false,
            0,
            None,
            false,
            false,
        ) {
            Ok(_) => panic!(
                "a 1-byte broadcast message must be rejected -- the alternating-last-two-bytes \
                 burst simulation would otherwise corrupt the address byte itself"
            ),
            Err(err) => err,
        };
        assert!(
            err.contains("3..=8"),
            "the rejection must cite the broadcast-specific 3..=8 range, not the ordinary \
             connection-bound 4..=4096 range: {err}"
        );
    }

    /// Fix 4 boundary check: exactly `3` bytes (1 address byte + 2 payload
    /// bytes) is the new floor and must be accepted -- the alternating pair
    /// (`Data[len-2]`, `Data[len-1]`) falls entirely within the payload at
    /// this length, never touching `Data[0]`.
    #[test]
    fn broadcast_message_at_exactly_the_three_byte_floor_is_accepted() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, 0xF3);
        let resolved = resolve_send_recv_tx(
            ChannelProtocol::TP2_0_PS,
            j2534_0404::PROTOCOL_TP2_0_PS,
            &params,
            &[],
            &[0x01, 0x02], // composed message is [0xF3, 0x01, 0x02], 3 bytes
            false,
            0,
            None,
            false,
            false,
        )
        .expect("a 3-byte broadcast message must be accepted -- it is exactly the new floor");
        assert_eq!(resolved.data, vec![0xF3, 0x01, 0x02]);
    }
}

/// ADR-192/Phase 7 Stage 7c: `apply_resolved_tx_flags` ORs in
/// `TX_FLAG_TP2_0_BROADCAST_MSG` only when a broadcast address is staged AND
/// the link is genuinely TP2.0 -- the same two-part gate `sw_can_tx_flags`/
/// `msg_priority_tx_flags` already require of their own ComParam-derived
/// bits.
#[cfg(test)]
mod apply_resolved_tx_flags_tp20_broadcast_tests {
    use super::*;

    #[test]
    fn broadcast_flag_set_when_address_staged_on_tp20_link() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, 0xF5);
        let flags =
            apply_resolved_tx_flags(0, None, &active, j2534_0404::PROTOCOL_TP2_0_PS, None, false);
        assert_eq!(
            flags & j2534_0404::TX_FLAG_TP2_0_BROADCAST_MSG,
            j2534_0404::TX_FLAG_TP2_0_BROADCAST_MSG
        );
    }

    #[test]
    fn broadcast_flag_unset_when_address_not_staged() {
        let active = ComParamSet::default();
        let flags =
            apply_resolved_tx_flags(0, None, &active, j2534_0404::PROTOCOL_TP2_0_PS, None, false);
        assert_eq!(flags & j2534_0404::TX_FLAG_TP2_0_BROADCAST_MSG, 0);
    }

    /// A broadcast address staged on the ComParam set must not bleed onto
    /// a non-TP2.0 link's TxFlags -- the same protocol-safety requirement
    /// `sw_can_tx_flags`'s own doc comment states for `PARAM_SW_CAN_HIGH_VOLTAGE`.
    #[test]
    fn broadcast_flag_unset_on_non_tp20_link_even_if_staged() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, 0xF5);
        let flags = apply_resolved_tx_flags(0, None, &active, j2534_0404::CAN, None, false);
        assert_eq!(flags & j2534_0404::TX_FLAG_TP2_0_BROADCAST_MSG, 0);
    }

    /// Codex review fix (P1, PR #101, round 5): a broadcast send on a CLL
    /// that ALSO has an established TP2.0 connection with a 29-bit TX-ID
    /// must not carry `TX_EXTENDED_ID` -- that flag describes the
    /// unrelated connection's own CAN-ID width, not the broadcast frame's.
    #[test]
    fn tx_extended_id_suppressed_for_broadcast_even_with_a_29bit_established_connection() {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TP20_BROADCAST_ADDRESS, 0xF5);
        let flags = apply_resolved_tx_flags(
            0,
            None,
            &active,
            j2534_0404::PROTOCOL_TP2_0_PS,
            Some(0x1FFF_FFFF),
            false,
        );
        assert_eq!(
            flags & j2534_0404::TX_FLAG_TP2_0_BROADCAST_MSG,
            j2534_0404::TX_FLAG_TP2_0_BROADCAST_MSG
        );
        assert_eq!(flags & j2534_0404::TX_EXTENDED_ID, 0);
    }

    /// The established-connection `TX_EXTENDED_ID` force-set must still
    /// apply normally for a non-broadcast send on the same kind of CLL --
    /// this fix must not regress the existing ADR-188 behavior.
    #[test]
    fn tx_extended_id_still_set_for_non_broadcast_send_with_a_29bit_established_connection() {
        let active = ComParamSet::default();
        let flags = apply_resolved_tx_flags(
            0,
            None,
            &active,
            j2534_0404::PROTOCOL_TP2_0_PS,
            Some(0x1FFF_FFFF),
            false,
        );
        assert_eq!(flags & j2534_0404::TX_FLAG_TP2_0_BROADCAST_MSG, 0);
        assert_eq!(
            flags & j2534_0404::TX_EXTENDED_ID,
            j2534_0404::TX_EXTENDED_ID
        );
    }
}

/// Everything `CoptStartcomm`'s periodic tester-present message needs,
/// resolved from `active` and the CLL's UniqueRespIdTable.
///
/// Always resolved from the call-time Active snapshot, regardless of whether
/// this COP's transient init transaction is borrowing Working
/// (`temp_param_update`, ADR-067 claim 8, née ADR-066): the periodic message
/// is a persistent product of the COP that outlives the transaction, so it
/// must never be built from a set that gets reverted before the COP
/// finishes -- otherwise a `temp_param_update` `CoptStartcomm` could leave an
/// ongoing periodic send running with Working values Working was never meant
/// to promote.
#[derive(Debug, Clone)]
pub(super) struct ResolvedTesterPresent {
    pub(super) data: Vec<u8>,
    pub(super) interval_ms: u32,
    pub(super) tx_flags: u32,
    pub(super) isotp_framing: Option<SoftIsoTpFraming>,
    /// `CP_TesterPresentSendType`: `0` = periodic, `1` = idle-triggered --
    /// both software-driven (ADR-083/this diff: mode 0 was previously
    /// hardware-autonomous via `PassThruStartPeriodicMsg`).
    pub(super) send_type: u32,
    /// Same `CP_P3Func`/`CP_P3Phys` addressing bucket selector
    /// `resolve_send_recv_tx` computes (`ResolvedSendRecvTx::can_functional`):
    /// `Some(true)`/`Some(false)` for a resolved functional/physical
    /// CAN-family addressing, `None` for a non-CAN-family protocol or
    /// unresolved addressing (ADR-083, gating the mode-0 periodic start the
    /// same way a `CoptSendrecv` write is gated, and the mode-1 idle one-shot
    /// send via `transmit_request`'s own `wait_for_p3_gap` call).
    pub(super) can_functional: Option<bool>,
    /// The decoded `CP_TesterPresentAddrMode` functional/physical reading
    /// (ADR-138 correction: `tx_header::use_functional_addressing(
    /// AddrModeSource::TesterPresent, active)`), resolved from the same
    /// call-time Active snapshot as every other field here. Added because
    /// `can_functional` above is populated only for CAN-family protocols
    /// (`protocol.is_can_family()`-gated) -- on KWP/J1850 it is always `None`
    /// regardless of `CP_TesterPresentAddrMode`, so a live addressing flip on
    /// those protocols would otherwise be invisible to `same_wire_behavior`
    /// whenever it also happens to produce byte-identical header bytes (a
    /// second Codex review round's finding: instance-level byte coincidence
    /// is not the same thing as the ComParam not affecting wire content).
    pub(super) addr_mode_functional: bool,
    /// The raw `base_tx_flags` this was resolved with, persisted so a later
    /// live `CoptUpdateparam` re-resolution (which has no client TxFlags
    /// source of its own for a persistent tester-present) can reuse it.
    pub(super) base_tx_flags: u32,
    /// The physical CAN ID(s) tester-present is addressed to, when
    /// resolvable (ADR-088 amendment): `Some(entries.first()`'s
    /// `CP_CanRespUSDTId`/`CP_CanRespUUDTId`)` when `can_functional ==
    /// Some(false)` (CAN-family physical) and `entries` is non-empty -- the
    /// same first-entry targeting rule `tx_header::build_tx_message`'s
    /// addressing already follows (ADR-050) -- else `None`
    /// (functional/broadcast, where every routed ECU is a legitimate reply,
    /// or non-CAN/no-table). **`TesterPresentState::Armed` (both
    /// `CP_TesterPresentSendType` values) reads this frozen field directly
    /// rather than recomputing it live** (`build_cll_rx_entries`,
    /// `events.rs`) -- an earlier draft recomputed it live on the assumption
    /// that the ADR-084 re-arm gate would always catch a relevant addressing
    /// change, which a second Codex review round found false:
    /// `same_wire_behavior` compares `data` (built from `CP_CanPhysReqId`,
    /// the request/TX addressing), not this field, so a `CoptUpdateparam`
    /// that changes only the response addressing (`CP_CanRespUSDTId`/
    /// `CP_CanRespUUDTId`) never re-arms, and the per-tick send itself never
    /// re-resolves `resolved` between arms either -- every send inside an
    /// already-open `CP_P2Max` window genuinely went out (and expects a
    /// reply) against whatever THIS field said at the last arm/re-arm, not
    /// whatever the live table currently says. Stored as the raw CAN ID(s)
    /// rather than the entry's `unique_resp_identifier` label specifically
    /// because the label itself is what a later table update can retarget (a
    /// separate, first Codex review round's finding, before this field
    /// existed in its current form).
    pub(super) target_can_ids: Option<TesterPresentTargetCanIds>,
    /// `CP_TesterPresentReqRsp == 1` (ADR-060/ADR-088 amendment): whether
    /// this tester-present send itself expects a response, classifying it
    /// for `CP_P3Func`/`CP_P3Phys` gap-timing purposes exactly like any
    /// other TX (`TxGapState::no_response_required = !expects_response`,
    /// `wait_for_p3_gap`'s `num_receive_cycles = expects_response as i32`
    /// truthiness). Resolved from the same call-time Active snapshot as
    /// every other field here -- see this struct's own top-level doc comment
    /// for why tester-present is never resolved from Working. Excluded from
    /// `same_wire_behavior`: it is an RX/gap-timing classification, not wire
    /// content, so it must not by itself trigger a re-arm/resend (same
    /// reasoning already applied to `target_can_ids`/`p2_max_ms`, and, as of
    /// the ADR-088 second amendment, `exp_pos_resp`/`exp_neg_resp` below).
    /// This same send-time snapshot also gates whether a `DiscardWindow`
    /// opens at all for a given send (ADR-088 second amendment): a send made
    /// while this was `false` never opens one, regardless of what
    /// `CP_TesterPresentReqRsp` says later.
    pub(super) expects_response: bool,
    /// `CoptSendrecv`'s own response-window duration, `CP_P2Max` (ADR-053),
    /// resolved from the SAME `active: &ComParamSet` this whole struct is
    /// resolved from -- always the call-time Active snapshot, even when
    /// `temp_param_update = 1` stages a different value in Working for the
    /// transient init transaction (ADR-067 claim 8: tester-present is a
    /// persistent product of this COP that outlives that transaction, so it
    /// must never be sized from a set that gets reverted before the COP
    /// finishes). Used to compute the `CP_P2Max` discard window pushed into
    /// `LogicalLinkState.open_tp_discards` (ADR-137 fourth Codex-review fix
    /// / round-4 restructure) at every one of its write sites --
    /// stored here specifically so `handle_start_comm`'s arm does not have
    /// to read `binding.resolved()` for it, which is the
    /// *Working* set under `temp_param_update = 1` (a Codex review finding:
    /// an earlier draft read `binding.resolved().p2_max_timeout_ms()` there,
    /// silently sizing the first discard window from a temporary,
    /// about-to-be-reverted value instead of the Active value tester-present
    /// itself was actually resolved against and will keep using thereafter).
    pub(super) p2_max_ms: u32,
    /// `CP_TesterPresentExpPosResp`, resolved from the same call-time Active
    /// snapshot as every other field here (ADR-088 second amendment).
    /// Stored here specifically so the two call sites that construct/replace
    /// `TesterPresentState::Armed` wholesale (`handle_start_comm`,
    /// `handle_update_param`) can freeze a `DiscardWindow`'s `pos` at the
    /// exact send instant, instead of `build_cll_rx_entries` re-reading
    /// `l.active.tester_present_exp_pos_resp()` live on every poll tick --
    /// the same live-vs-snapshot drift `p2_max_ms`/`expects_response` were
    /// already fixed for in the first ADR-088 amendment applied here too, a
    /// gap a later Codex review round found this field had not yet closed.
    /// Excluded from `same_wire_behavior` for the same reason `p2_max_ms`/
    /// `expects_response`/`target_can_ids` are: it drives RX discard
    /// classification, not wire content, so a `CoptUpdateparam` that changes
    /// only this ComParam must not by itself re-arm/resend.
    pub(super) exp_pos_resp: Vec<u8>,
    /// `CP_TesterPresentExpNegResp`; same resolution/freeze/exclusion
    /// rationale as `exp_pos_resp` above.
    pub(super) exp_neg_resp: Vec<u8>,
    /// `CP_TesterPresentHandling == 1` (ADR-137): the master enable switch
    /// for periodic/idle-triggered tester-present, resolved from the same
    /// call-time Active snapshot as every other field here. Checked FIRST in
    /// `resolve_tester_present`, before any fallible resolution (message/
    /// header construction, `CP_TesterPresentTime` rounding,
    /// `CP_TesterPresentSendType` range validation) -- a Codex-review fix:
    /// an earlier version of this field was computed last, so a disabled
    /// `CP_TesterPresentHandling` combined with any other invalid
    /// tester-present ComParam (e.g. an out-of-range `CP_TesterPresentSendType`)
    /// made resolution fail with `Err` before this field was ever read,
    /// which made `handle_update_param`'s disarm-on-disable arm never fire
    /// (an `Err` just emits `PduErrEvtTesterPresentError` and leaves the
    /// prior `Armed` state running) and could reject `CoptStartcomm` outright
    /// even though handling was off. Since handling is the master switch, an
    /// explicit disable must always succeed and disarm regardless of what
    /// otherwise-unused tester-present configuration is sitting in Active --
    /// so this field, and only this field, is resolved before every other
    /// fallible step, with every other field vacuous when it comes out
    /// `false` (see `resolve_tester_present`'s early-return branch). This
    /// field is therefore always truthful regardless of which path
    /// resolution took, same as before this fix, just checked earlier.
    /// Excluded from `same_wire_behavior` for the same reason
    /// `expects_response`/`exp_pos_resp`/`exp_neg_resp`/`target_can_ids`/
    /// `p2_max_ms` are: it is a gate on *whether* tester-present runs, not
    /// wire content. Both call sites (`handle_start_comm`,
    /// `handle_update_param`) branch on it explicitly instead.
    pub(super) handling_enabled: bool,
}

impl ResolvedTesterPresent {
    /// Whether `self` and `other` describe the same on-wire tester-present
    /// behavior (ADR-084): used by `handle_update_param`'s live re-arm gate
    /// to tell "this `CoptUpdateparam` actually changed something
    /// tester-present-affecting" apart from "an unrelated ComParam (e.g.
    /// `CP_Loopback`) was promoted on a comm-started, mode-1 CLL" -- the
    /// latter must not re-send a frame or reset the idle clock.
    /// Deliberately excludes `base_tx_flags`: it is bookkeeping for a later
    /// re-resolution, not itself part of what goes on the wire (a `tx_flags`
    /// comparison already covers any actual TxFlags difference). Also
    /// excludes `target_can_ids`, `p2_max_ms`, `expects_response`, and (ADR-088
    /// second amendment) `exp_pos_resp`/`exp_neg_resp`: all five drive RX
    /// discard/gap-timing classification, not wire content or send cadence --
    /// the same reasoning that keeps `CP_TesterPresentReqRsp`/`ExpPosResp`/
    /// `ExpNegResp` themselves out of this gate. A `CP_P2Max`-only (or
    /// `target_can_ids`-only, or `ExpPosResp`/`ExpNegResp`-only)
    /// `CoptUpdateparam` on an already-armed CLL therefore does not re-arm or
    /// re-send -- `handle_update_param`'s `Ok(resolved) if ...` guard fails,
    /// and its `Ok(_) => {}` fallthrough leaves `TesterPresentState::Armed`
    /// (including its stored `resolved`) untouched, discarding the
    /// freshly-resolved value entirely. This is `dispatch_due_tester_present`'s
    /// job to correct, not this struct's: per-tick sends never consult
    /// `resolved.p2_max_ms`/`expects_response`/`exp_pos_resp`/`exp_neg_resp`
    /// at all -- they capture `CP_P2Max`/`CP_TesterPresentReqRsp`/
    /// `CP_TesterPresentExpPosResp`/`CP_TesterPresentExpNegResp` fresh from
    /// live Active immediately before their own send (ADR-088 amendment's
    /// own read-timing-consistency fix), freezing that same snapshot into
    /// the resulting `DiscardWindow` so a live re-read after the fact can
    /// never drift from what the send instant actually expected (ADR-088
    /// second amendment) -- so a change to any of them still takes effect on
    /// the very next tick despite not re-arming. `p2_max_ms`/
    /// `expects_response`/`exp_pos_resp`/`exp_neg_resp` on this struct are
    /// consulted only by the two call sites that construct/replace `Armed`
    /// wholesale (`handle_start_comm`'s arm, `handle_update_param`'s own
    /// re-arm branch), where they are by definition fresh (this struct was
    /// just resolved moments earlier).
    ///
    /// Includes `addr_mode_functional` (ADR-138 correction) alongside
    /// `send_type`/`can_functional`: `can_functional` alone is populated only
    /// for CAN-family protocols, so on KWP/J1850 a live
    /// `CP_TesterPresentAddrMode` flip is otherwise visible to this
    /// comparison only via `data`, and `data` can coincidentally stay
    /// byte-identical when a CLL's `CP_PhysReqFormatPriorityType`/
    /// `CP_PhysReqTargetAddr` happen to be explicitly configured identical to
    /// `CP_FuncReqFormatPriorityType`/`CP_FuncReqTargetAddr`. NOTE 1 ties the
    /// resend trigger to the ComParam value changing, not to whether the
    /// resulting bytes happen to coincide for one particular configuration,
    /// so `addr_mode_functional` is compared directly rather than relying on
    /// `data`/`can_functional` to catch every case.
    pub(super) fn same_wire_behavior(&self, other: &ResolvedTesterPresent) -> bool {
        self.data == other.data
            && self.interval_ms == other.interval_ms
            && self.tx_flags == other.tx_flags
            && self.isotp_framing == other.isotp_framing
            && self.send_type == other.send_type
            && self.can_functional == other.can_functional
            && self.addr_mode_functional == other.addr_mode_functional
    }
}

/// Resolves the tester-present message content/interval/TxFlags/ISO-TP
/// framing from `active` (ADR-050: payload-only, header-prefixed the same
/// way `CoptSendrecv` is) and `entries` (the CLL's UniqueRespIdTable
/// snapshot). An empty configured payload means "no tester-present" and
/// short-circuits to `Ok` with an empty `data` -- no addressing is required
/// to send nothing. `CP_TesterPresentHandling == 0` (ADR-137 amendment) is
/// checked FIRST, before any other field is resolved, and short-circuits to
/// a vacuous, all-disabled `Ok` the same way -- see the master-switch check
/// immediately below for why. Takes `hw_protocol_id` (the link's actual
/// connected hardware protocol, not its service-level `ChannelProtocol`) for
/// header construction, matching `resolve_send_recv_tx`'s `hw_protocol`.
///
/// Called from `rpc_start_com_primitive`, synchronously, against the
/// call-time Active snapshot (ADR-067; reverting ADR-066's deferral of this
/// call to the poll task). A resolution failure there (e.g. a missing
/// addressing ComParam for a non-empty tester-present payload) is a
/// synchronous `StartComPrimitive` `INVALID_ARGUMENT`, as it was before
/// ADR-066.
///
/// Also called a second time, from the poll task, by `handle_update_param`
/// (ADR-084): a live `CoptUpdateparam` that promotes tester-present-affecting
/// ComParams to Active while the CLL's `comm_started` is already `true`
/// re-resolves tester-present the same way, against the newly-promoted
/// Active set, to decide whether to re-arm and send immediately. A
/// resolution failure there is surfaced asynchronously as
/// `PduErrEvtTesterPresentError`, not a synchronous error (there is no RPC
/// call in flight to fail at that point).
pub(super) fn resolve_tester_present(
    protocol: ChannelProtocol,
    hw_protocol_id: u32,
    active: &ComParamSet,
    entries: &[EcuUniqueRespEntry],
    software_isotp: bool,
    base_tx_flags: u32,
    // ADR-196 Decision items 1/2: the calling CLL's `LogicalLinkState::
    // raw_mode` -- see `resolve_send_recv_tx`'s identical parameter for the
    // full rationale. TesterPresent is unreachable for TP2.0 regardless (see
    // this function's own `build_tx_message` call below), and Phase 1's
    // protocol allowlist means `raw_mode` is only ever `true` for a base CAN
    // or hardware ISO15765 link, so the two never conflict.
    raw_mode: bool,
) -> Result<ResolvedTesterPresent, String> {
    // ADR-137 Codex-review fix: the master enable switch is checked FIRST,
    // before any fallible resolution below (message/header construction,
    // `CP_TesterPresentTime` rounding, `CP_TesterPresentSendType` range
    // validation). Handling off means nothing gets sent, so none of that
    // otherwise-unused configuration needs to be valid -- an explicit
    // disable must always succeed and disarm, never fail resolution because
    // an unrelated tester-present ComParam happens to be out of range. Every
    // other field is vacuous here: nothing reads them once `handling_enabled
    // == false` (`handle_start_comm`'s `None` branch, `handle_update_param`'s
    // leading disarm match arm), so there is no live-Active-snapshot
    // discipline to preserve for them in this branch.
    if active.tester_present_handling() != 1 {
        // ADR-138 correction: infallible (a plain `HashMap` lookup +
        // comparison), so computing it here does not reopen the
        // master-switch-before-any-fallible-resolution ordering rule above.
        let addr_mode_functional =
            tx_header::use_functional_addressing(tx_header::AddrModeSource::TesterPresent, active);
        return Ok(ResolvedTesterPresent {
            data: Vec::new(),
            interval_ms: 0,
            tx_flags: base_tx_flags,
            isotp_framing: None,
            send_type: 0,
            can_functional: None,
            addr_mode_functional,
            base_tx_flags,
            target_can_ids: None,
            expects_response: false,
            p2_max_ms: 0,
            exp_pos_resp: Vec::new(),
            exp_neg_resp: Vec::new(),
            handling_enabled: false,
        });
    }

    let can_addressing = tx_header::resolve_can_addressing(
        tx_header::AddrModeSource::TesterPresent,
        active,
        entries,
    );
    // Same ISO15765 addressing derivation `resolve_send_recv_tx` uses for its
    // own `tx_addressing` -- reused below both for the software-ISO-TP
    // `isotp_framing` and (ADR-215) `sae_tx_size_range`'s
    // `extended_addressing` input / the hardware-ISO15765 functional Single
    // Frame check.
    let tx_addressing = can_addressing.map_or(isotp::Addressing::Normal, |a| a.tx_addressing);
    // ADR-138 correction: the decoded CP_TesterPresentAddrMode reading,
    // independent of `can_functional`'s CAN-family gating -- see
    // `ResolvedTesterPresent::addr_mode_functional`'s own doc comment.
    let addr_mode_functional =
        tx_header::use_functional_addressing(tx_header::AddrModeSource::TesterPresent, active);
    // Same CP_P3Func/P3Phys addressing-bucket derivation as
    // `resolve_send_recv_tx` (ADR-060/ADR-083): CAN-family only.
    //
    // Codex review, PR #107 round 5: under RawMode, derived from
    // `addr_mode_functional` (`CP_TesterPresentAddrMode` read directly)
    // rather than `can_addressing.map(|a| a.functional)` -- a RawMode
    // client's `CP_TesterPresentMessage` already embeds its own literal CAN
    // ID (Decision item 2), so it has no reason to configure
    // `CP_CanFuncReqId`/a physical `UniqueRespIdTable` entry either, leaving
    // `can_addressing` `None` for a supported RawMode tester-present
    // configuration. `wait_for_p3_gap`'s `CP_P3Func` enforcement and
    // `tester_present_tx_can_id`'s CAN-ID extraction (needed to recognize
    // this send's own loopback/TX-indication frames so they don't leak into
    // the client's receive stream) both key off this field. RawMode=OFF is
    // unchanged.
    let can_functional = protocol.is_can_family().then(|| {
        if raw_mode {
            addr_mode_functional
        } else {
            can_addressing.is_some_and(|a| a.functional)
        }
    });
    let tp_payload = active.tester_present_data();
    let data = if tp_payload.is_empty() {
        Vec::new()
    } else {
        // Header construction must follow the actual connected hardware
        // protocol, not service-level identity -- see `resolve_send_recv_tx`'s
        // `hw_protocol` for why (ADR-046/ADR-070/pin-typing amendment).
        // ADR-157 Plane B: normalized, same reasoning as
        // `resolve_send_recv_tx`'s `hw_protocol`. Bound to a variable (rather
        // than inlined into the `build_tx_message` call, as before) so the
        // ADR-215 checks below can reuse it.
        let hw_protocol = ChannelProtocol::from_raw(resources::base_protocol_id(hw_protocol_id));
        let mut built = tx_header::build_tx_message(
            hw_protocol,
            tx_header::AddrModeSource::TesterPresent,
            active,
            entries,
            tp_payload,
            software_isotp,
            // TP2.0's `is_tp20_param` allowlist (`comparam_support.rs`)
            // excludes `CP_TesterPresentSendType`, and
            // `PARAM_TESTER_PRESENT_HANDLING` is not in that allowlist
            // either -- it can therefore never be staged nonzero on a TP2.0
            // CLL, so `handling_enabled` above is always `false` and this
            // whole branch is unreachable for TP2.0 (ADR-188 section 4: no
            // client-driven periodic keep-alive concept exists for it).
            None,
            raw_mode,
        )
        .map_err(|err| format!("tester-present message: {err}"))?;

        // ADR-215 Decision item 2: `CP_TesterPresentMessage` composes a full
        // J2534 TX message (header + payload) the same way an ordinary
        // `CoptSendrecv` does, so the identical SAE J2534-1 per-protocol
        // composed-message size range `resolve_send_recv_tx` already
        // enforces for those sends (factored into the shared
        // `sae_tx_size_range` helper above) is enforced here too -- this
        // closes `j2534-0404-service/docs/implementation-notes.md`'s former
        // P2 backlog entry ("no length validation anywhere in this service,
        // for any protocol").
        let tx_prefix = compute_tx_prefix(raw_mode, hw_protocol_id, base_tx_flags, &built);
        let fd_link = !software_isotp && resources::is_fd_protocol_id(hw_protocol_id);
        let fd_base_family = fd_link.then(|| resources::base_protocol_id(hw_protocol_id));
        // Same RawMode-vs-non-RawMode derivation `resolve_send_recv_tx` uses
        // for its own `extended_addressing`.
        let extended_addressing = if raw_mode {
            tx_prefix == 5
        } else {
            !software_isotp && matches!(tx_addressing, isotp::Addressing::Extended(_))
        };
        let sae_range = sae_tx_size_range(
            hw_protocol,
            extended_addressing,
            fd_base_family,
            active,
            // `checksum_mode` is not threaded into this function, and
            // TP2.0-broadcast is unreachable for tester-present -- see
            // `sae_tx_size_range`'s own doc comment for why both are always
            // `false` here.
            false,
            false,
        );
        if !sae_range.contains(&built.len()) {
            return Err(format!(
                "CP_TesterPresentMessage composes a {}-byte J2534 message, outside the \
                 valid TX message size range ({}..={} bytes) for this protocol \
                 (SAE J2534-1 Figure 42)",
                built.len(),
                sae_range.start(),
                sae_range.end(),
            ));
        }

        // Codex review, PR #107 round 7: unlike the non-RawMode path (whose
        // `build_tx_message` always PREPENDS a real header derived from
        // ComParams, so the result is inherently well-formed), RawMode's
        // early-return returns `tp_payload` byte-for-byte unchanged with no
        // validation at all -- ordinary CoptSendrecv sends get a synchronous
        // SAE J2534-1 TX size-range check (`resolve_send_recv_tx`), matched
        // above (ADR-215). This narrower, RawMode-specific check additionally
        // enforces the prefix minimum the general range check alone cannot
        // catch (a message can be within the overall valid length range yet
        // still shorter than its own required raw CAN-ID prefix), since
        // `compute_tx_prefix` already resolves to `0` for the Analog
        // Inputs/SCI no-op exception (no minimum to enforce there).
        if raw_mode && built.len() < tx_prefix {
            return Err(format!(
                "CP_TesterPresentMessage length {} is shorter than the {tx_prefix}-byte \
                 RawMode CAN-ID prefix this protocol/addressing configuration requires",
                built.len()
            ));
        }

        // ADR-215 Decision item 3: an FD-connected `FD_CAN_PS` link's
        // composed tester-present message is padded to the next DLC-legal
        // length, mirroring `resolve_send_recv_tx`'s identical
        // `CP_CanFillerByte`-driven padding (ADR-159) exactly -- reached only
        // once the range check above has already confirmed the unpadded
        // length fits the link's staged `CP_CANFDTxMaxDataLength`, so padding
        // never grows the message past its own ceiling. `FD_ISO15765_PS` is
        // excluded: the native adapter's own ISO15765 state machine
        // pads/segments its own frames, so this service must not double-pad.
        if fd_base_family == Some(j2534_0404::CAN) {
            let data_len = built.len() - 4;
            if data_len > 8 {
                let padded_len = 4 + fd_can_padded_data_len(data_len);
                let filler = active
                    .unum32
                    .get(&PARAM_CAN_FILLER_BYTE)
                    .copied()
                    .unwrap_or(0) as u8;
                built.resize(padded_len, filler);
            }
        }

        // ADR-215 Decision item 4: ISO 15765-2 requires a single-frame fit
        // for both addressing paths tester-present can use.
        if software_isotp {
            // Software ISO-TP (ADR-046): mirrors `frame_tester_present_data`'s
            // own shape check (`events.rs`) exactly -- `built` is
            // `[4-byte CAN ID][payload]`, never carrying an AE byte
            // regardless of addressing. `frame_tester_present_data`'s check
            // is kept in place as defense-in-depth, but is now structurally
            // unreachable in the normal case: any payload that would fail it
            // already fails here, at `CoptStartcomm`/`CoptUpdateparam`
            // resolution time.
            let max_payload = tx_addressing.max_sf_payload();
            if built.len() < 5 || built.len() - 4 > max_payload {
                return Err(format!(
                    "CP_TesterPresentMessage composes a {}-byte software ISO-TP message \
                     ([4-byte CAN ID][{}-byte payload]), exceeding the {max_payload}-byte \
                     Single Frame limit ISO 15765-2 requires for this addressing mode",
                    built.len(),
                    built.len().saturating_sub(4),
                ));
            }
        } else if hw_protocol == ChannelProtocol::ISO15765 && can_functional == Some(true) {
            // Hardware ISO15765 with functional addressing (ADR-055): mirrors
            // `resolve_send_recv_tx`'s identical functional Single Frame
            // check.
            let is_fd_iso15765 = fd_base_family == Some(j2534_0404::ISO15765);
            let functional_addressing = if raw_mode {
                if extended_addressing {
                    isotp::Addressing::Extended(0)
                } else {
                    isotp::Addressing::Normal
                }
            } else {
                tx_addressing
            };
            let max_sf_payload = if is_fd_iso15765 {
                functional_addressing.fd_max_sf_payload(effective_fd_tx_dl(active))
            } else {
                functional_addressing.max_sf_payload()
            };
            let functional_payload_len = tp_payload.len().saturating_sub(tx_prefix);
            if functional_payload_len > max_sf_payload {
                return Err(format!(
                    "CP_TesterPresentMessage length {} ({}-byte payload after the \
                     {tx_prefix}-byte RawMode prefix) exceeds the {max_sf_payload}-byte Single \
                     Frame limit for functional (CP_TesterPresentAddrMode=1) ISO15765 \
                     addressing -- ISO 15765-2 requires functionally addressed requests to fit \
                     in a single frame{}",
                    tp_payload.len(),
                    functional_payload_len,
                    if is_fd_iso15765 {
                        " (see this link's staged CP_CANFDTxMaxDataLength)"
                    } else {
                        ""
                    },
                ));
            }
        }

        built
    };

    // Software ISO-TP mode (ADR-046 addendum): addressing for the wrapped
    // tester-present SingleFrame comes from the same UniqueRespIdTable entry
    // used to build `data` above (ADR-050); Normal when the table is empty.
    // Reuses the `tx_addressing` computed earlier (ADR-215) instead of
    // re-deriving it a second time.
    let isotp_framing = if software_isotp {
        Some(SoftIsoTpFraming {
            framing: active.isotp_framing(0),
            addressing: tx_addressing,
        })
    } else {
        None
    };

    // Same TxFlags corrections as CoptSendrecv (ADR-046, ADR-062): strip the
    // software-driven FRAME_PAD flag from the raw CAN channel, then let the
    // resolved addressing / SCI ComParams override whatever the client
    // requested for the four objective-fact bit positions.
    let mut tx_flags = base_tx_flags;
    if software_isotp {
        tx_flags &= !j2534_0404::TX_ISO15765_FRAME_PAD;
    }
    // `None`: TesterPresent is unreachable for TP2.0 (see `data`'s own
    // `build_tx_message` call above, `handling_enabled` is always `false`
    // for a TP2.0 CLL).
    tx_flags = apply_resolved_tx_flags(
        tx_flags,
        can_addressing,
        active,
        hw_protocol_id,
        None,
        raw_mode,
    );

    // ADR-158 correction (Codex review PR #30 round 4): same objective-fact
    // reasoning as `resolve_send_recv_tx`'s FD flags just above -- an
    // FD-connected link's tester-present frame is still an FD-format frame.
    // The size-range/padding gap this comment used to flag as open (`CP_
    // TesterPresentMessage` had no length validation anywhere in this
    // service, for any protocol) is now closed by ADR-215: `data`'s own
    // construction above (inside the `data` block, before `isotp_framing`)
    // already validates the composed message against SAE J2534-1's
    // per-protocol TX size range (`sae_tx_size_range`) and pads an
    // `FD_CAN_PS` link's message to the next DLC-legal length, mirroring
    // this exact FD-flags logic's own reasoning.
    if !software_isotp && resources::is_fd_protocol_id(hw_protocol_id) {
        tx_flags |= j2534_0404::TX_FD_CAN_FORMAT;
        if active
            .unum32
            .get(&PARAM_CANFD_BAUDRATE)
            .copied()
            .unwrap_or(0)
            != 0
        {
            tx_flags |= j2534_0404::TX_FD_CAN_BRS;
        }
    }

    // CP_TesterPresentTime is µs-resolution (ISO 22900-2), like every other
    // CP_* timing param; the software due-check interval this resolves to is
    // native J2534 ms resolution, matching what PassThruStartPeriodicMsg's
    // TimeInterval argument used to require for mode 0 before this diff
    // removed it (ADR-083, applying the ADR-072 `us_to_ms` conversion
    // pattern). A raw stored `0` means "disabled" and is checked BEFORE
    // conversion so that sentinel is untouched; a non-zero raw value that
    // rounds to 0 ms (sub-500us) is rejected here rather than silently
    // collapsing a configured keep-alive into "disabled".
    let raw_interval_us = active.tester_present_interval_us();
    let interval_ms = if raw_interval_us == 0 {
        0
    } else {
        let ms = us_to_ms(raw_interval_us);
        if ms == 0 {
            return Err(format!(
                "CP_TesterPresentTime ({raw_interval_us} us) rounds to 0 ms, \
                 which would silently disable tester-present"
            ));
        }
        ms
    };

    // CP_TesterPresentSendType only has two defined ISO 22900-2 values (0:
    // periodic, 1: idle-triggered); SetComParam does not range-check it, so
    // an out-of-range value must be rejected here rather than silently
    // falling into the dispatch logic's `else` (idle-mode) branch.
    let send_type = active.tester_present_send_type();
    if send_type != 0 && send_type != 1 {
        return Err(format!(
            "CP_TesterPresentSendType ({send_type}) is not a supported value (must be 0 or 1)"
        ));
    }

    // ADR-088 amendment: the physical CAN ID(s) tester-present is addressed
    // to, when resolvable -- see `ResolvedTesterPresent::target_can_ids`'s
    // own doc comment for why mode 0 needs this frozen here, as raw CAN
    // IDs rather than a `unique_resp_identifier` label, rather than
    // recomputed live.
    let target_can_ids = (can_functional == Some(false))
        .then(|| entries.first())
        .flatten()
        .map(|e| TesterPresentTargetCanIds {
            usdt: e.params.unum32.get(&PARAM_CAN_RESP_USDT_ID).copied(),
            uudt: e.params.unum32.get(&PARAM_CAN_RESP_UUDT_ID).copied(),
        });

    // ADR-088 amendment (Codex review finding): from `active`, the same
    // Active snapshot every other field here resolves from -- never
    // `binding`/Working, even under `temp_param_update = 1` -- see
    // `ResolvedTesterPresent::p2_max_ms`'s own doc comment.
    let p2_max_ms = active.p2_max_timeout_ms();

    // Same live-Active-snapshot discipline as p2_max_ms above -- see
    // `ResolvedTesterPresent::expects_response`'s own doc comment.
    let expects_response = active.tester_present_req_rsp() == 1;

    // ADR-088 second amendment: same live-Active-snapshot discipline as
    // p2_max_ms/expects_response above -- see
    // `ResolvedTesterPresent::exp_pos_resp`'s own doc comment.
    let exp_pos_resp = active.tester_present_exp_pos_resp().to_vec();
    let exp_neg_resp = active.tester_present_exp_neg_resp().to_vec();

    // ADR-137 (Codex-review fix): always `true` here -- the `!= 1` case
    // already returned at the top of this function, before any of the
    // fallible resolution above ran.
    Ok(ResolvedTesterPresent {
        data,
        interval_ms,
        tx_flags,
        isotp_framing,
        send_type,
        can_functional,
        addr_mode_functional,
        base_tx_flags,
        target_can_ids,
        expects_response,
        p2_max_ms,
        exp_pos_resp,
        exp_neg_resp,
        handling_enabled: true,
    })
}

#[cfg(test)]
mod resolve_tester_present_addr_mode_tests {
    //! ADR-138 correction (Codex review, PR #158): a KWP-family CLL whose
    //! `CP_PhysReqFormatPriorityType`/`CP_PhysReqTargetAddr` happen to be
    //! explicitly configured identical to `CP_FuncReqFormatPriorityType`/
    //! `CP_FuncReqTargetAddr` produces byte-identical KWP headers for a
    //! physical vs. functional `CP_TesterPresentAddrMode` resolution.
    //! `can_functional` is `None` on this (non-CAN-family) protocol, so
    //! before this fix `same_wire_behavior` saw no difference at all between
    //! the two resolutions. This module reproduces that exact byte
    //! coincidence and confirms `same_wire_behavior` now still tells them
    //! apart via the new `addr_mode_functional` field.
    use super::*;

    /// A KWP (ISO14230) `ComParamSet` with tester-present enabled and a
    /// non-empty payload, and `CP_PhysReq*`/`CP_FuncReq*` deliberately
    /// configured identical so physical and functional resolutions produce
    /// byte-identical headers -- the byte-coincidence premise this test
    /// depends on.
    fn byte_coincident_kwp_active(addr_mode: Option<u32>) -> ComParamSet {
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TESTER_PRESENT_HANDLING, 1);
        active
            .bytes
            .insert(PARAM_TESTER_PRESENT_MSG, vec![0x3E, 0x00]);
        active.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x80);
        active.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
        active.unum32.insert(PARAM_FUNC_REQ_FORMAT_PRIORITY, 0x80);
        active.unum32.insert(PARAM_FUNC_REQ_TARGET_ADDR, 0x10);
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
        if let Some(mode) = addr_mode {
            active.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, mode);
        }
        active
    }

    #[test]
    fn byte_coincident_kwp_addr_mode_flip_produces_identical_data() {
        let physical = byte_coincident_kwp_active(None);
        let functional = byte_coincident_kwp_active(Some(1));

        let resolved_physical = resolve_tester_present(
            ChannelProtocol::ISO14230,
            j2534_0404::ISO14230,
            &physical,
            &[],
            false,
            0,
            false,
        )
        .unwrap();
        let resolved_functional = resolve_tester_present(
            ChannelProtocol::ISO14230,
            j2534_0404::ISO14230,
            &functional,
            &[],
            false,
            0,
            false,
        )
        .unwrap();

        // Confirms the byte-coincidence premise: identical KWP header bytes
        // despite the addressing ComParam having flipped.
        assert_eq!(resolved_physical.data, resolved_functional.data);
        assert!(!resolved_physical.addr_mode_functional);
        assert!(resolved_functional.addr_mode_functional);
        // The fix: same_wire_behavior must NOT report these as the same wire
        // behavior, despite identical `data` and both `can_functional`
        // fields being `None` (non-CAN-family).
        assert_eq!(resolved_physical.can_functional, None);
        assert_eq!(resolved_functional.can_functional, None);
        assert!(!resolved_physical.same_wire_behavior(&resolved_functional));
    }
}

/// Resolves the J2534 TxFlags for the K-line fast-init wakeup frame
/// (`run_protocol_init`'s `PassThruMessage`), from `params` -- the bound
/// `ParamBinding::resolved()` set (Active ordinarily, or Working for the
/// duration of a `temp_param_update` `CoptStartcomm`'s transient init
/// transaction, ADR-067 née ADR-066). Unlike `resolve_tester_present`, this
/// is scoped entirely to the init step and never outlives it, so it is free
/// to read whichever set the binding names. Called eagerly from
/// `rpc_start_com_primitive` (ADR-067); never fails.
pub(super) fn resolve_init_tx_flags(
    params: &ComParamSet,
    entries: &[EcuUniqueRespEntry],
    base_tx_flags: u32,
    hw_protocol_id: u32,
    // ADR-196 Decision items 1/2, extended by ADR-198 Phase 2: the calling
    // CLL's `LogicalLinkState::raw_mode`. This is the K-line fast-init
    // wakeup frame -- `raw_mode` CAN now be `true` here (K-line RawMode is
    // accepted at `CreateComLogicalLink`), but it remains a functional
    // no-op for this call site: `apply_resolved_tx_flags`'s own RawMode
    // branch only ever changes CAN-specific bits
    // (`TX_EXTENDED_ID`/`ISO15765_ADDR_TYPE`) that are never set for a
    // K-line `hw_protocol_id` in the first place (see that function's own
    // doc comment). Threaded through for the same defense-in-depth reason
    // `apply_resolved_tx_flags`'s own doc comment gives.
    raw_mode: bool,
) -> u32 {
    let can_addressing =
        tx_header::resolve_can_addressing(tx_header::AddrModeSource::Request, params, entries);
    // `None`: this is the K-line fast-init wakeup frame, never reached for
    // a TP2.0 (CAN-based) link.
    apply_resolved_tx_flags(
        base_tx_flags,
        can_addressing,
        params,
        hw_protocol_id,
        None,
        raw_mode,
    )
}

/// Outcome of `J2534Service::revalidate_tp20_broadcast_periodic_reservation`
/// (Codex review round 15 Fix 2, P1, PR #101, ADR-193).
///
/// The two non-`Ok` shapes are deliberately distinct, and they differ in far
/// more than the `Status` they carry: they decide WHO still owns this
/// cop_handle's cleanup. A reservation that is simply GONE means some
/// terminator already took it and, with it, sole responsibility for
/// eventually finalizing this cop_handle -- so this RPC must neither report a
/// failure for it nor touch `self.primitives`, the same "the loser doesn't
/// re-report (or dismantle) an already-resolved COP" reasoning
/// `finalize_or_orphan_broadcast_periodic_start_locked`'s `NotOwned`
/// resolution already applies. Any OTHER broken predicate leaves the
/// reservation still nominally this cop_handle's own, so this call keeps the
/// cleanup duty and rolls back both halves itself; it is genuinely rejected,
/// exactly as `reserve_tp20_broadcast_periodic`'s own initial check would
/// have rejected it a moment earlier, so it carries that same `Status`.
enum BroadcastPeriodicRevalidation {
    /// Every predicate still holds -- proceed with the temp-param apply and
    /// the native `start_periodic_message` call.
    Ok,
    /// This cop_handle's own `None`-sentinel reservation is gone (taken by a
    /// terminator, replaced by another cop_handle, or the CLL vanished
    /// entirely): abort the start and report the RPC successful, WITHOUT
    /// rolling anything back -- there is nothing left in `logical_links` to
    /// clear, and the `primitives` entry belongs to whichever terminator took
    /// the reservation (see the call site's own comment, and
    /// `emit_terminal_if_live`'s presence gate, for what removing it from
    /// under that terminator used to cost).
    AlreadyResolved,
    /// The reservation is still nominally this cop_handle's own, but another
    /// predicate broke while this call was blocked on `self.api`: abort the
    /// start, roll back (reservation AND `primitives` entry), and return this
    /// rejection.
    Rejected(Box<Status>),
}

/// How `J2534Service::finalize_or_orphan_broadcast_periodic_start_locked`
/// resolved a just-succeeded native `start_periodic_message` call against the
/// `None`-sentinel reservation `reserve_tp20_broadcast_periodic` wrote (Codex
/// review, PR #101, round 5; lifted to module scope by round 15's
/// serialization fix, ADR-193, so the `self.api`-guarded resolution half and
/// the post-`drop(api)` bookkeeping half can be separate methods). See
/// `finalize_or_orphan_broadcast_periodic_start_locked`'s own doc comment for
/// what each variant means and why.
enum BroadcastPeriodicStartResolution {
    NotOwned,
    Live {
        queue_target: Option<events::CllQueueTarget>,
    },
    ClearedByPendingGeneration,
}

impl J2534Service {
    /// Snapshot of `handle`'s current Working ComParam set, Working
    /// UniqueRespIdTable, AND live raw `hw_protocol_id` (Plane A, ADR-157),
    /// taken together under a SINGLE `logical_links` lock acquisition -- the
    /// same TOCTOU-closing pattern `rpc_start_com_primitive` uses for
    /// `bound_comparams`/`bound_active_table` (ADR-067 claim A, ADR-068): a
    /// `SetComParam` or `SetUniqueRespIdTable` landing between two separate
    /// reads could otherwise hand `CoptUpdateparam` a mixed-time (params,
    /// table) pair, promoting a Working snapshot that never actually existed
    /// together at any single instant. `hw_protocol_id` is immutable within a
    /// connect generation (only `apply_fd_mode`/J1850 autodetect write it,
    /// both inside `rpc_connect_com_logical_link` before `connected`), so
    /// reading it in this same snapshot -- rather than the top-of-function
    /// `link` clone taken before Working was staged -- costs nothing and
    /// keeps the FD-mode guard below (ADR-158 correction, round 3) reading a
    /// value from the exact same instant as `params`. Used only by
    /// `rpc_start_com_primitive`'s `CoptUpdateparam` branch.
    ///
    /// The 4th element, `channel_key`, is this CLL's own
    /// `LogicalLinkState::channel_key` from the same snapshot instant
    /// (ADR-178): the Analog Inputs `CoptUpdateparam` guard uses it to look
    /// up this CLL's `SharedChannel` (and its recorded
    /// `applied_analog_sample_rate`) in a separate, later
    /// `shared_channels` acquisition -- sequential, not nested, since this
    /// function's own `logical_links` guard is already dropped by the time
    /// that lookup runs.
    async fn update_param_working_snapshot(
        &self,
        handle: u32,
    ) -> (
        ComParamSet,
        Vec<EcuUniqueRespEntry>,
        u32,
        Option<ChannelKey>,
        bool,
        u32,
        u64,
    ) {
        self.logical_links
            .lock()
            .await
            .get(&handle)
            .map(|l| {
                (
                    l.working.clone(),
                    l.working_unique_resp_id_table.clone(),
                    l.hw_protocol_id,
                    l.channel_key,
                    l.comm_started,
                    l.active
                        .unum32
                        .get(&ComParamId(j2534_0404::NODE_ADDRESS))
                        .copied()
                        .unwrap_or(0xF1),
                    l.connect_generation,
                )
            })
            .unwrap_or_default()
    }

    /// Rolls back a CoptStopcomm RPC's `self.primitives` entry unconditionally
    /// and clears `stop_comm_pending` ONLY if the live `LogicalLinkState` is
    /// still on the same `connect_generation` this RPC captured -- a
    /// disconnect+reconnect completing while this rollback was suspended on
    /// either lock must not clear a NEW CoptStopcomm's own guard flag
    /// (Codex review, PR #92).
    async fn rollback_stop_comm_pending(
        &self,
        handle: u32,
        cop_handle: u32,
        connect_generation: u64,
    ) {
        self.primitives.lock().await.remove(&cop_handle);
        if let Some(link) = self.logical_links.lock().await.get_mut(&handle)
            && link.connect_generation == connect_generation
        {
            link.stop_comm_pending = false;
        }
    }

    /// Fix 2 (Codex review, P1, PR #101, ADR-192/Phase 7 Stage 7c): rolls
    /// back this `cop_handle`'s own `None`-sentinel `tp20_broadcast_periodic`
    /// reservation (see that field's own doc comment) -- called from every
    /// failure path between the reservation write and the native
    /// `start_periodic_message` call returning `Ok`, so a failed start does
    /// not permanently block this CLL from ever starting a real one. Only
    /// clears the entry if it is STILL this exact `cop_handle`'s own
    /// `None`-sentinel reservation -- defensive against a concurrent
    /// `CoptCancel`/teardown having already raced in and cleared or
    /// replaced it (see those sites' own comments for how they treat the
    /// sentinel).
    ///
    /// **Not called when the start already KNOWS it lost the race**
    /// (edge-case-hunter finding, round 15 follow-up, ADR-193): the
    /// `BroadcastPeriodicRevalidation::AlreadyResolved` exit in
    /// `rpc_start_com_primitive` deliberately skips this helper entirely. The
    /// `logical_links` half would be a no-op there by definition, and the
    /// unconditional `primitives` removal above is outright WRONG once
    /// another path owns this cop_handle's finalization -- every terminator's
    /// own terminal-status emission is gated on that entry still being
    /// present, so removing it makes the COP vanish with no status and no
    /// `terminal_cops` record. This helper stays correct only for the paths
    /// that still own the reservation themselves (a `Rejected` revalidation,
    /// a message-construction failure, a failed temp-param apply, a failed
    /// native start).
    ///
    /// Field-wise match, not a full-struct equality check (sentinel
    /// deferral fix, edge-case-hunter finding, design-advisor-approved):
    /// ownership is decided by `cop_handle` and `message_id.is_none()`
    /// alone -- `pending_clear_generation`'s value is ignored here. A racing
    /// `CLEAR_PERIODIC_MSGS` scan may have already stamped a nonzero
    /// `pending_clear_generation` onto this exact reservation
    /// (`rpc_misc.rs`'s reconciliation scan); that still means "this is
    /// still MY reservation, just one a clear has since scanned", not "not
    /// mine". A literal full-struct comparison against
    /// `{ message_id: None, started_epoch: 0 }` (with no
    /// `pending_clear_generation` field) would silently stop matching a
    /// stamped reservation once the native start then fails, leaving a
    /// stale sentinel stuck forever -- blocking every future start on this
    /// CLL and blocking `LOCK_PHYSICAL_TX_QUEUE` grants.
    async fn rollback_tp20_broadcast_periodic_reservation(&self, handle: u32, cop_handle: u32) {
        self.primitives.lock().await.remove(&cop_handle);
        if let Some(link) = self.logical_links.lock().await.get_mut(&handle)
            && link
                .tp20_broadcast_periodic
                .is_some_and(|p| p.cop_handle == cop_handle && p.message_id.is_none())
        {
            link.tp20_broadcast_periodic = None;
        }
    }

    /// Fix 2 + Fix 3 (Codex review, P1, PR #101, ADR-192/Phase 7 Stage 7c):
    /// the merged suspension/staleness/already-active check AND reservation
    /// write for a TP2.0 broadcast periodic start, all in ONE `logical_links`
    /// critical section -- see `rpc_start_com_primitive`'s own broadcast-
    /// periodic branch comment for the full TOCTOU rationale this closes.
    ///
    /// `snapshot_connect_generation` is the `LinkView` snapshot's own
    /// `connect_generation` (`link.connect_generation`, captured much
    /// earlier in the call by `get_link_state`) -- re-verified here, live,
    /// against the SAME `logical_links` read this function uses for the
    /// suspension/already-active checks and the reservation write, mirroring
    /// the ADR-086 `connect_generation` re-verification idiom already used
    /// earlier in this same function (`if connect_generation !=
    /// link.connect_generation`, above). `connected` is checked too, not
    /// just `connect_generation`: a plain disconnect (no reconnect) leaves
    /// `connect_generation` unchanged (`rpc_link.rs`'s disconnect path only
    /// clears `connected`/`channel_id`, it does not bump the generation --
    /// only a finalized (re)connect does), so `connect_generation` alone
    /// would miss that case.
    ///
    /// On success, returns the LIVE `(channel_id, hw_protocol_id)` to use for
    /// the native call the caller makes afterward -- read from this SAME
    /// critical section, never from the (potentially stale by now) `LinkView`
    /// snapshot, so even a same-generation-but-somehow-different `channel_id`
    /// read is impossible.
    async fn reserve_tp20_broadcast_periodic(
        &self,
        handle: u32,
        cop_handle: u32,
        snapshot_connect_generation: u64,
    ) -> Result<(ChannelId, u32), Status> {
        let mut links = self.logical_links.lock().await;
        match links.get_mut(&handle) {
            Some(l) if l.connect_generation != snapshot_connect_generation || !l.connected => {
                Err(state_guard_status(
                    Code::FailedPrecondition,
                    "the ComLogicalLink was disconnected and reconnected while this call was in \
                     progress; retry against the current connection",
                    PduError::PduErrCllNotConnected,
                    l.last_error.clone(),
                ))
            }
            Some(l) if l.tx_suspended() => Err(Status::failed_precondition(
                "cannot start a TP2.0 broadcast periodic re-trigger while this ComLogicalLink's \
                 TX dispatch is suspended (PDU_IOCTL_SUSPEND_TX_QUEUE, a sibling CLL's \
                 LOCK_PHYSICAL_TX_QUEUE, or CP_SuspendQueueOnError) -- unlike an ordinary \
                 ComPrimitive, a native periodic message cannot be queued for later dispatch; \
                 retry once the suspension clears",
            )),
            Some(l) if l.tp20_broadcast_periodic.is_some() => Err(Status::failed_precondition(
                "a TP2.0 broadcast periodic re-trigger is already active on this ComLogicalLink \
                 -- cancel it first before starting another",
            )),
            Some(l) => match l.channel_id {
                Some(channel_id) => {
                    l.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                        cop_handle,
                        message_id: None,
                        started_epoch: 0,
                        pending_clear_generation: 0,
                    });
                    Ok((channel_id, l.hw_protocol_id))
                }
                None => Err(Status::internal(
                    "logical link is connected but channel_id is not set",
                )),
            },
            // The CLL vanished from the map entirely between the pre-flight
            // snapshot and here -- treated the same as a generation mismatch
            // (matching `rpc_start_com_primitive`'s own CoptStopcomm branch
            // precedent, which folds "vanished" into its generation check via
            // `link.as_ref().map(|l| l.connect_generation) != Some(connect_generation)`),
            // not as "already active".
            None => Err(state_guard_status(
                Code::FailedPrecondition,
                "the ComLogicalLink was disconnected and reconnected while this call was in \
                 progress; retry against the current connection",
                PduError::PduErrCllNotConnected,
                None,
            )),
        }
    }

    /// Re-runs `reserve_tp20_broadcast_periodic`'s full predicate set a
    /// SECOND time, from inside `rpc_start_com_primitive`'s own already-held
    /// `self.api` guard and before that bracket's temp-param apply and native
    /// `start_periodic_message` call (Codex review round 15 Fix 2, P1, PR
    /// #101, ADR-193 partially superseding ADR-192 Decision item 2's
    /// in-flight-reservation mechanism).
    ///
    /// **Why a second check.** The reservation is written under
    /// `self.logical_links` alone and released again; `self.api` is only
    /// acquired afterward. A terminator (`CoptCancel`, a TX-suspension
    /// termination, or a CLL teardown) needs nothing but `logical_links` to
    /// take that `None`-sentinel entry, so before this fix it could take it
    /// AND report the COP terminal to the client while this call was still
    /// blocked on `self.api` -- and then `PassThruStartPeriodicMsg` ran
    /// anyway, emitting SAE J2534-2 clause 19.3.2.3's five-frame burst
    /// synchronously inside the native call. The later best-effort orphan
    /// stop cannot retract frames already on the wire, so traffic was
    /// transmitted after a cancellation or after a successful queue
    /// suspension.
    ///
    /// ADR-193's fix makes `self.api`'s mutex the serialization fence for
    /// in-flight broadcast-periodic starts: every terminator now re-inspects
    /// (and takes) the sentinel only while holding `self.api`
    /// (`take_broadcast_periodic_under_api_locked`, `rpc_misc.rs`, and
    /// `terminate_tp20_broadcast_periodic_for_suspension`'s own unconditional
    /// `self.api` acquisition), so exactly one of the two orderings can
    /// happen: either the terminator wins the fence and this revalidation
    /// then observes its own reservation gone (no native start ever runs), or
    /// this bracket wins and the terminator waits behind `self.api` until the
    /// start has committed a real `message_id` it can stop for real.
    ///
    /// Called with `api` held: the `logical_links` read below is therefore
    /// nested INSIDE that guard, which is the ADR-110-sanctioned order (`api`
    /// outer, `logical_links` inner) this same function already uses for its
    /// temp-param apply and its `revert_hardware_to_live_active_locked`
    /// calls. The parameter is the caller's guard deref, taken purely as
    /// proof the fence is held -- exactly the shape
    /// `apply_params_to_hardware_locked`/`revert_hardware_to_live_active_locked`
    /// already use.
    ///
    /// The predicates mirror `reserve_tp20_broadcast_periodic`'s ownership/
    /// session checks (see that function's own doc comment for why each one
    /// is checked, including why `connected` is checked separately from
    /// `connect_generation`) -- not an exact mirror: this omits `reserve`'s
    /// own `channel_id.is_some()` arm, benignly, since every site that clears
    /// `channel_id` also clears `connected` in the same critical section, so
    /// the `connected` check alone already catches it. The rejection
    /// `Status`es are the same ones `reserve_tp20_broadcast_periodic` returns
    /// for the shared predicates, so a start
    /// rejected here is indistinguishable to the client from one the initial
    /// reservation attempt had rejected. Ownership is tested first, because
    /// "someone else already resolved this COP" is reported as success rather
    /// than as a rejection -- see [`BroadcastPeriodicRevalidation`].
    async fn revalidate_tp20_broadcast_periodic_reservation(
        &self,
        _api: &J2534Api0404,
        handle: u32,
        cop_handle: u32,
        snapshot_connect_generation: u64,
    ) -> BroadcastPeriodicRevalidation {
        let links = self.logical_links.lock().await;
        let Some(link) = links.get(&handle) else {
            // The CLL vanished entirely (`DestroyComLogicalLink`), taking the
            // reservation with it -- definitionally "not mine anymore", so it
            // is the already-resolved case, not a rejection.
            return BroadcastPeriodicRevalidation::AlreadyResolved;
        };
        // Field-wise ownership match, ignoring `pending_clear_generation`,
        // for the same reason `rollback_tp20_broadcast_periodic_reservation`
        // does (see its doc comment): a racing `CLEAR_PERIODIC_MSGS` scan may
        // legitimately have stamped this exact reservation, which still means
        // "still mine".
        if !link
            .tp20_broadcast_periodic
            .is_some_and(|p| p.cop_handle == cop_handle && p.message_id.is_none())
        {
            return BroadcastPeriodicRevalidation::AlreadyResolved;
        }
        if link.connect_generation != snapshot_connect_generation || !link.connected {
            return BroadcastPeriodicRevalidation::Rejected(Box::new(state_guard_status(
                Code::FailedPrecondition,
                "the ComLogicalLink was disconnected and reconnected while this call was in \
                 progress; retry against the current connection",
                PduError::PduErrCllNotConnected,
                link.last_error.clone(),
            )));
        }
        if link.tx_suspended() {
            return BroadcastPeriodicRevalidation::Rejected(Box::new(Status::failed_precondition(
                "cannot start a TP2.0 broadcast periodic re-trigger while this ComLogicalLink's \
                 TX dispatch is suspended (PDU_IOCTL_SUSPEND_TX_QUEUE, a sibling CLL's \
                 LOCK_PHYSICAL_TX_QUEUE, or CP_SuspendQueueOnError) -- unlike an ordinary \
                 ComPrimitive, a native periodic message cannot be queued for later dispatch; \
                 retry once the suspension clears",
            )));
        }
        BroadcastPeriodicRevalidation::Ok
    }

    /// Reconciles a just-succeeded native `start_periodic_message` call
    /// against the `None`-sentinel reservation
    /// `reserve_tp20_broadcast_periodic` wrote earlier: overwrites the
    /// sentinel with the real `Some(message_id)` if this cop_handle's
    /// reservation is still intact (Fix 2, Codex review, PR #101) -- but
    /// ONLY if the entry is STILL this exact cop_handle's own reservation.
    /// Nothing else should be able to have raced in given the reservation
    /// above, but a concurrent `CoptCancel`/CLL teardown/suspension
    /// termination/`CLEAR_PERIODIC_MSGS` could have taken/cleared it while
    /// this native call was in flight (each of those sites treats a
    /// `None`-sentinel entry as "a start is in flight, nothing real to stop
    /// yet" -- see their own comments); if that happened, resurrecting the
    /// entry here would make a just-cancelled COP look live again.
    ///
    /// If the reservation was lost (Fix 1, Codex review, P1, PR #101,
    /// ADR-192/Phase 7 Stage 7c): the native message DID just start
    /// successfully device-side, and with the reservation gone, nothing
    /// anywhere still points to it -- permanently orphaned, not even
    /// reachable via `disconnect_com_logical_link`'s shared-channel leak
    /// tracking, since nothing ever pushed it there either. Best-effort stop
    /// it now -- the "if I lost the race, clean up after myself" pattern
    /// `rpc_cancel_com_primitive`'s own best-effort `PassThruStopPeriodicMsg`
    /// failure handling already uses (search "Fix 2 (Codex review round 2,
    /// P2, PR #101)" in this same file). Issued AFTER releasing
    /// `self.logical_links` (this call needs `self.api`), matching this
    /// mechanism's existing lock-ordering discipline throughout. Accepted
    /// residual, documented as its own P3 bullet in
    /// the Prioritized Backlog: a failure here
    /// is a rare double-fault -- the message is now genuinely leaked
    /// device-side with no channel left to leak-track it on, since this
    /// method has no `shared_channels` lock in scope and adding that
    /// lock-order dependency is out of scope for this fix.
    ///
    /// Fix 2 (Codex review, P2, PR #101, round 5): when the reservation was
    /// lost (not owned), this method does NOT mark the COP `dispatched` or
    /// emit `PduCopstExecuting` -- the losing side already reported (or is
    /// about to report) this cop_handle terminal (CoptCancel's own
    /// `Cancelled`, a suspension termination's `Cancelled`,
    /// `CLEAR_PERIODIC_MSGS`' `Finished`, or teardown removing the COP
    /// outright), so an unconditional `Executing` here would surface a
    /// nonterminal status for a COP this call no longer owns, observable as
    /// e.g. `Cancelled` immediately followed by a bogus `Executing` for the
    /// same cop_handle. The RPC itself still succeeds either way (a periodic
    /// message genuinely started, even if this particular cop_handle no
    /// longer claims it) -- the caller's own `Ok(Response)` tail (including
    /// the unconditional `temp_param_update && temp_eligible`
    /// Working-writeback) runs regardless of what this method does.
    ///
    /// Sentinel deferral fix (edge-case-hunter finding, design-advisor-
    /// approved): ownership alone is no longer enough to decide "commit
    /// live" -- a `CLEAR_PERIODIC_MSGS` scan that ran while this native call
    /// was still in flight cannot resolve a still-in-flight reservation's
    /// ordering against its own native `clear_periodic_messages` call (no
    /// `started_epoch` exists yet to compare), so instead of finalizing it
    /// stamps `pending_clear_generation` on the reservation and leaves it
    /// tracked (`rpc_misc.rs`'s reconciliation scan). This method is where
    /// that deferred decision is actually made, now that `started_epoch` is
    /// known: an OWNED reservation branches three ways --
    ///
    /// - **Not owned** (entry gone, or belongs to a different cop_handle):
    ///   the orphan/best-effort-stop path above, unchanged.
    /// - **Owned, `started_epoch >= pending_clear_generation`** (covers
    ///   `pending_clear_generation == 0`, i.e. no clear ever scanned this
    ///   reservation): no recorded clear's native call ran after this
    ///   start's own native call returned, so the message is still live
    ///   device-side -- commit it live exactly as before.
    /// - **Owned, `started_epoch < pending_clear_generation`**: at least one
    ///   recorded clear's native `clear_periodic_messages` call ran AFTER
    ///   this start's own native call returned -- that channel-wide clear
    ///   already killed this message device-side, so no per-id
    ///   `stop_periodic_message` is issued here (mirroring the reasoning in
    ///   `rpc_misc.rs`'s own "Fix 2" comment: a channel-wide clear already
    ///   did the device-side equivalent). The entry is taken (never
    ///   committed, `dispatched` never set, no `Executing` emitted) and
    ///   `PduCopstFinished` is emitted via `events::emit_terminal_if_live`
    ///   after `self.logical_links` is released -- the same helper
    ///   `CLEAR_PERIODIC_MSGS`'s own scan uses for its take-and-finalize
    ///   path, reproducing exactly what that scan would have emitted had it
    ///   been able to resolve the sentinel at scan time. Accepted transient
    ///   (ADR-192): between the scan recording `pending_clear_generation`
    ///   and this native call resolving, `GetStatus` briefly still reports
    ///   `Executing` for a COP whose message may already be cleared
    ///   device-side -- bounded by the in-flight native call's own
    ///   duration, not indefinite.
    ///
    /// **Round 15 split (Codex review, P1, PR #101, ADR-193).** This method
    /// is the `self.api`-GUARDED half: the `logical_links` resolution below,
    /// and the `NotOwned` orphan stop it can issue, both run inside
    /// `rpc_start_com_primitive`'s own already-held `api` guard, immediately
    /// after the native `start_periodic_message` call returns and BEFORE that
    /// guard is dropped. That is what makes the commit of a real
    /// `Some(message_id)` (or the orphan stop of a message nobody owns
    /// anymore) visible to every terminator the instant it can acquire
    /// `self.api` -- terminators now take a `None`-sentinel reservation only
    /// under that same fence (see
    /// `revalidate_tp20_broadcast_periodic_reservation`'s doc comment for the
    /// full ADR-193 design). Before the split, this resolution ran after
    /// `drop(api)` and re-acquired both locks itself, leaving a window in
    /// which a terminator could observe neither the sentinel it had already
    /// taken nor the real id this call had not yet committed. The remaining
    /// bookkeeping -- `primitives`/`dispatched` and the status emissions --
    /// deliberately does NOT run here; it is
    /// `finalize_broadcast_periodic_start_bookkeeping`'s job, called by the
    /// same caller once `api` is dropped, since none of it needs the fence
    /// and this crate holds `api` no longer than necessary.
    ///
    /// `api` is the caller's guard deref, taken purely as proof the fence is
    /// held (the same `_locked` shape `apply_params_to_hardware_locked` and
    /// `revert_hardware_to_live_active_locked` use); the `logical_links` lock
    /// taken below is therefore nested inside it, the ADR-110-sanctioned
    /// order.
    async fn finalize_or_orphan_broadcast_periodic_start_locked(
        &self,
        api: &J2534Api0404,
        handle: u32,
        cop_handle: u32,
        channel_id: ChannelId,
        message_id: j2534_0404::PeriodicMessageId,
        started_epoch: u64,
    ) -> BroadcastPeriodicStartResolution {
        use BroadcastPeriodicStartResolution as Resolution;

        let resolution = {
            let mut links = self.logical_links.lock().await;
            match links.get_mut(&handle) {
                Some(l) => {
                    let owned = l
                        .tp20_broadcast_periodic
                        .is_some_and(|p| p.cop_handle == cop_handle && p.message_id.is_none());
                    if !owned {
                        Resolution::NotOwned
                    } else {
                        let pending_clear_generation = l
                            .tp20_broadcast_periodic
                            .expect("just checked is_some_and above")
                            .pending_clear_generation;
                        if started_epoch < pending_clear_generation {
                            l.tp20_broadcast_periodic = None;
                            Resolution::ClearedByPendingGeneration
                        } else {
                            l.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                                cop_handle,
                                message_id: Some(message_id),
                                started_epoch,
                                pending_clear_generation: 0,
                            });
                            Resolution::Live {
                                queue_target: Some(events::CllQueueTarget::from_link(l)),
                            }
                        }
                    }
                }
                None => Resolution::NotOwned,
            }
        };
        if let Resolution::NotOwned = &resolution {
            match api.stop_periodic_message(channel_id, message_id) {
                Ok(()) => warn!(
                    cll_handle = handle,
                    cop_handle,
                    message_id = message_id.0,
                    "stopped an orphaned TP2.0 broadcast periodic message: its cop_handle's \
                     `None`-sentinel reservation was taken or cleared by a concurrent \
                     CoptCancel/teardown while the native start was in flight"
                ),
                Err(err) => warn!(
                    cll_handle = handle,
                    cop_handle,
                    message_id = message_id.0,
                    %err,
                    "failed to stop an orphaned TP2.0 broadcast periodic message after \
                     losing the in-flight reservation race -- the message is now leaked \
                     device-side with no tracking left anywhere to stop it later, or the \
                     message was already stopped by a concurrent \
                     CLEAR_PERIODIC_MSGS/teardown"
                ),
            }
        }
        resolution
    }

    /// The post-`drop(api)` half of the round-15 split (Codex review, P1, PR
    /// #101, ADR-193): everything
    /// `finalize_or_orphan_broadcast_periodic_start_locked` deliberately does
    /// NOT do under the `self.api` fence, because none of it needs that fence
    /// -- the `primitives`/`dispatched` update and the COP status emissions
    /// for the `Live`/`ClearedByPendingGeneration` resolutions. `NotOwned` is
    /// fully handled by the locked half (its orphan stop) and by whichever
    /// terminator took the reservation (which reports this cop_handle
    /// terminal itself), so it emits nothing here -- see this pair's shared
    /// doc comment above for why an unconditional `Executing` would be wrong.
    async fn finalize_broadcast_periodic_start_bookkeeping(
        &self,
        handle: u32,
        cop_handle: u32,
        message_id: j2534_0404::PeriodicMessageId,
        resolution: BroadcastPeriodicStartResolution,
    ) {
        use BroadcastPeriodicStartResolution as Resolution;

        match resolution {
            Resolution::NotOwned => {}
            Resolution::ClearedByPendingGeneration => {
                warn!(
                    cll_handle = handle,
                    cop_handle,
                    message_id = message_id.0,
                    "TP2.0 broadcast periodic start resolved against a CLEAR_PERIODIC_MSGS \
                     that raced ahead of it: the channel-wide clear's native call already \
                     invalidated this message device-side before this start's own native call \
                     returned, so no per-id stop_periodic_message is issued"
                );
                events::emit_terminal_if_live(
                    &self.primitives,
                    &self.logical_links,
                    &self.subscriptions,
                    &self.terminal_cops,
                    handle,
                    cop_handle,
                    vci_service_interface::PduComPrimitiveStatus::PduCopstFinished,
                )
                .await;
            }
            Resolution::Live { queue_target } => {
                // `GetStatus(CopHandle)`'s own derivation (ADR-117) also
                // consults `dispatched` to distinguish `Waiting` from `Idle`
                // in the fallback branch it never reaches for this COP
                // (`rpc_get_status`'s own `is_broadcast_periodic` check
                // reports `Executing` directly) -- kept accurate here
                // anyway, matching every other dispatch path's own
                // bookkeeping discipline, in case that field is ever
                // consulted for this COP kind by a future change.
                if let Some(entry) = self.primitives.lock().await.get_mut(&cop_handle) {
                    entry.dispatched = true;
                }
                // A single confirmation event at start -- no per-cycle
                // events for a broadcast periodic COP (ADR-192
                // Consequences: the native `PassThruStartPeriodicMsg`/
                // `PassThruStopPeriodicMsg` pair gives this service no
                // per-tick visibility, matching ADR-083's original
                // tester-present mode-0 acceptance). It stays
                // `PduCopstExecuting` (per `GetStatus`) until a later
                // `CoptCancel`/teardown/`CLEAR_PERIODIC_MSGS` transitions it
                // -- unlike an ordinary infinite cyclic send, no
                // `PduCopstFinished` is ever emitted for it here.
                //
                // Routed through `emit_nonterminal_if_live` rather than a
                // direct `send_cop_status` call (Codex review, P2, PR #101,
                // round 12, ADR-192): the locked half's own `logical_links`
                // critical section already committed this COP's real entry
                // (replacing the `None`-sentinel reservation) before
                // releasing that lock -- and, as of round 15's split
                // (ADR-193), before `self.api` was dropped too -- opening a
                // window where a concurrent
                // `CLEAR_PERIODIC_MSGS`/suspension-termination/`CoptCancel`
                // can see the now-live entry, take it, stop it natively,
                // remove it from `primitives`, and emit its own terminal
                // status before this deferred `Executing` report runs. See
                // `emit_nonterminal_if_live`'s own doc comment
                // (`events_event_senders.rs`) for the full race and why
                // `primitives` containment alone is a sufficient liveness
                // signal for this COP kind.
                events::emit_nonterminal_if_live(
                    &self.primitives,
                    &self.subscriptions,
                    &self.terminal_cops,
                    queue_target.as_ref(),
                    handle,
                    cop_handle,
                    vci_service_interface::PduComPrimitiveStatus::PduCopstExecuting,
                )
                .await;
            }
        }
    }

    pub(super) async fn rpc_start_com_primitive(
        &self,
        request: Request<vci_service_interface::StartComPrimitiveRequest>,
    ) -> Result<Response<vci_service_interface::ComPrimitiveResponse>, Status> {
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;
        let link = self.get_link_state(handle).await?;
        if !link.connected {
            return Err(state_guard_status(
                Code::FailedPrecondition,
                "logical link must be connected before starting a primitive",
                PduError::PduErrCllNotConnected,
                link.last_error,
            ));
        }

        let cop_type = vci_service_interface::ComOperationType::try_from(request.cop_type)
            .unwrap_or(vci_service_interface::ComOperationType::CoptUnspecified);

        if cop_type == vci_service_interface::ComOperationType::CoptUnspecified {
            return Err(Status::invalid_argument(
                "cop_type must not be COPT_UNSPECIFIED",
            ));
        }

        // ADR-204: enforce the tag size cap before any `CopEntry` is
        // constructed (and well before the native call) -- the tag is
        // echoed on every COP-status event this COP emits over its whole
        // lifetime, so an unbounded tag would be a per-event amplification
        // hazard `MAX_COP_TAG_LEN` exists to bound.
        if let Some(tag) = request.cop_tag.as_ref()
            && tag.len() > MAX_COP_TAG_LEN
        {
            return Err(Status::invalid_argument(format!(
                "cop_tag exceeds the maximum size of {MAX_COP_TAG_LEN} bytes"
            )));
        }

        // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): a broader
        // sibling of the Analog Inputs gate further below -- clause 24 bars
        // EVERY ComPrimitive type unconditionally (clause 24.2.5.2-5.5:
        // PassThruReadMsgs/WriteMsgs/StartPeriodicMsg/StartMsgFilter are all
        // rejected), with no receive-only exemption at all (unlike Analog
        // Inputs, whose whole point is reading). Checked here, before the
        // `comm_started` pre-flight state checks just below and before
        // `cop_handle` allocation -- unconditionally for every `cop_type`,
        // including `CoptStartcomm`/`CoptStopcomm` regardless of this CLL's
        // current `comm_started` state, since clause 24 defines no COP-level
        // API surface at all for this protocol to have a meaningful
        // started/stopped state in the first place. Placed ahead of (not
        // alongside) the narrower Analog Inputs gate, which is only reached
        // once `transmits` is classified further down and so cannot run this
        // early.
        if link.hw_protocol_id == j2534_0404::PROTOCOL_ETHERNET_NDIS {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: no ComPrimitive type is supported on a SAE J2534-2 \
                 clause 24 Ethernet_NDIS link -- clause 24 defines this protocol as \
                 connect/disconnect plus one info IOCTL only, with no COP-level API surface at \
                 all",
                PduError::PduErrIdNotSupported,
                link.last_error,
            ));
        }

        // Pre-flight state checks: enforce comm_started invariants before allocating
        // a cop_handle so that no cleanup is needed on early return.
        match cop_type {
            vci_service_interface::ComOperationType::CoptStartcomm if link.comm_started => {
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "comm is already started; issue CoptStopcomm before starting again",
                    PduError::PduErrCllConnected,
                    link.last_error,
                ));
            }
            vci_service_interface::ComOperationType::CoptStopcomm if !link.comm_started => {
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "comm is not started; CoptStopcomm requires a prior successful CoptStartcomm",
                    PduError::PduErrCllNotStarted,
                    link.last_error,
                ));
            }
            _ => {}
        }

        // ADR-067 claims D/E: temp_param_update is only meaningful for these
        // three COP types. `temp_param_update` is read once, here, since it
        // gates both the BUSTYPE guard below and the Working writeback at the
        // end of this call.
        let temp_param_update = request
            .cop_ctrl_data
            .as_ref()
            .map(|c| c.temp_param_update != 0)
            .unwrap_or(false);
        let temp_eligible = matches!(
            cop_type,
            vci_service_interface::ComOperationType::CoptSendrecv
                | vci_service_interface::ComOperationType::CoptStartcomm
                | vci_service_interface::ComOperationType::CoptStopcomm
        );

        // ADR-067 claim A (and the TOCTOU fix it requires): bind the
        // ComParam snapshot for this COP's whole life in a SINGLE critical
        // section, here, early -- before any lock check, cop_handle
        // allocation, or enqueue. A prior version of this code re-acquired
        // `logical_links` a second time, later, inside the CoptSendrecv/
        // CoptStartcomm branches, via a since-removed per-COP-type snapshot
        // helper, to build the actual `ParamBinding` -- leaving a window
        // between the BUSTYPE guard's
        // read below and that later read in which a concurrent
        // `SetComParam` (mutating `working` under the same lock, e.g.
        // DATA_RATE -- itself a BUSTYPE param) could land, letting a temp
        // COP bind a physical-bus change the guard was meant to reject.
        // Capturing both here, once, and threading the same clones through
        // every use below (the guard, and -- for CoptSendrecv/CoptStartcomm
        // -- the `ParamBinding`/`effective` construction) closes that gap:
        // nothing below ever re-reads `working`/`active` live for this
        // COP's own resolution. (The temp hardware REVERT target is the one
        // deliberate exception: it is read live at revert time in the poll
        // task -- see `ParamBinding::Temp`.)
        //
        // ADR-068 (same TOCTOU class, same fix): `active_unique_resp_id_table`
        // is cloned in this SAME critical section, unconditionally (not
        // gated on `temp_eligible`) -- a prior version called the separate
        // `unique_resp_id_table_snapshot` helper later, inside the
        // CoptSendrecv/CoptStartcomm branches, under its own lock
        // acquisition. That left a window in which a concurrent promotion
        // (this CLL's own `CoptUpdateparam` executing, or another CLL's
        // `ConnectComLogicalLink` -- Active can change independently of
        // this RPC call) could swap Active between the two reads, handing
        // this COP a comparams-from-T1 / table-from-T2 mixed snapshot.
        // `bound_active_table` is used unmodified by the CoptSendrecv/
        // CoptStartcomm branches below in place of that helper.
        // `connect_generation` is captured unconditionally in this same
        // critical section, mirroring `active_table` above -- it is only
        // consumed by the CoptStartcomm branch below (folded into
        // TxItem::StartComm), but reading it here, at this point (after the
        // comm_started-already-started precondition check above, so it
        // reflects the connection this COP is actually being accepted
        // against), keeps the read atomic with the rest of this call-time
        // snapshot rather than adding a second, later, racy lock acquisition
        // just for CoptStartcomm (ADR-086, mirroring ADR-067's discipline).
        let (
            bound_comparams,
            bound_active_table,
            connect_generation,
            last_error_at_snapshot,
            j1939_negotiated_unclaimed_at_bind,
            tp20_established_tx_id,
        ) = {
            let links = self.logical_links.lock().await;
            let link_state = links.get(&handle);
            let comparams = if temp_eligible {
                link_state.map(|l| (l.working.clone(), l.active.clone()))
            } else {
                None
            };
            let active_table = link_state
                .map(|l| l.active_unique_resp_id_table.clone())
                .unwrap_or_default();
            let generation = link_state.map(|l| l.connect_generation).unwrap_or(0);
            let last_error = link_state.and_then(|l| l.last_error.clone());
            // ADR-180 Decision 14 (design-advisor consult, PR #72 round 12,
            // Finding 1 Part A): captured in this SAME critical section as
            // `bound_comparams`/`connect_generation` above, not re-read
            // later -- the same TOCTOU-avoidance discipline ADR-067 already
            // established for this block: a live `LogicalLinkState` (not
            // the earlier `LinkView` snapshot, which has no
            // `j1939_claimed_address`/`active` fields) is only in scope
            // here.
            //
            // `edge-case-hunter` finding (round 12 verification pass): reads
            // the SAME ComParamSet `resolve_send_recv_tx`'s own
            // `j1939_tx_source` computation will use for THIS call --
            // Working when `temp_param_update` stages a Temp binding for
            // this one CoptSendrecv (mirroring the `ParamBinding::Temp`
            // choice made below at `binding`'s own construction), else
            // Active -- not unconditionally Active. Reading Active
            // unconditionally let this gate and `resolve_send_recv_tx`
            // disagree about whether this CLL is even "negotiated" for a
            // Temp-bound send whose staged `CP_J1939AddressNegotiationRule`
            // differs from Active's, per `j1939_negotiated_unclaimed_for`'s
            // own doc comment.
            let j1939_unclaimed = link_state.is_some_and(|l| {
                let temp_effective = (temp_param_update && temp_eligible)
                    .then(|| comparams.as_ref().map(|(working, _)| working))
                    .flatten();
                events::j1939_negotiated_unclaimed_for(l, temp_effective.unwrap_or(&l.active))
            });
            // Codex review fix (PR #97, ADR-188): the TX-ID `build_tx_message`'s
            // TP2.0 arm frames onto must come from this CLL's real,
            // `logical_links`-tracked connection phase -- captured in this
            // SAME critical section as everything else above, mirroring
            // `bound_comparams`'s own TOCTOU discipline -- never from a
            // client-writable ComParam.
            let tp20_tx_id = link_state.and_then(|l| l.tp20_connection).and_then(|c| {
                (c.phase == Tp20ConnectionPhase::Established)
                    .then_some(c.established_tx_id)
                    .flatten()
            });
            (
                comparams,
                active_table,
                generation,
                last_error,
                j1939_unclaimed,
                tp20_tx_id,
            )
        };

        // ADR-086 (round 5): this block's `connect_generation` read is a
        // SECOND, separate `logical_links` lock acquisition from the
        // `link` snapshot captured near the top of this function. If a
        // disconnect+reconnect of this same `cll_handle` completes between
        // those two reads, `link` (used below to build `tx`'s
        // protocol/hw_protocol_id/software_isotp) is stale relative to
        // `bound_comparams`/`bound_active_table`/`connect_generation`
        // captured here from the live map. Reject synchronously -- before
        // any cop_handle allocation, so no cleanup is needed -- rather than
        // let a COP bind a mixed-time (old link, new generation) snapshot.
        // Applies uniformly to every cop_type that reaches this shared
        // block (CoptSendrecv/CoptStartcomm get the same one-connection
        // guarantee as CoptStopcomm, for free).
        if connect_generation != link.connect_generation {
            return Err(state_guard_status(
                Code::FailedPrecondition,
                "the ComLogicalLink was disconnected and reconnected while this call was in \
                 progress; retry against the current connection",
                PduError::PduErrCllNotConnected,
                last_error_at_snapshot,
            ));
        }

        // ADR-067 claim E: a temp_param_update call must not stage a
        // PDU_PC_BUSTYPE-class ComParam change (a bus-physical param that is
        // not meaningful to change for just one COP's transaction) --
        // rejected synchronously, before any lock check, enqueue, or
        // Working/Active side effect. Per ADR-110, this is now the SOLE
        // call-time physical-ComParam gate for temp_param_update: the
        // separate LOCK_PHYSICAL_COM_PARAMS check that used to run
        // immediately after this guard is removed (a live lock conflict no
        // longer has anything left to protect against once this guard has
        // already rejected any staged BUSTYPE difference). Checked against
        // `bound_comparams` above -- the exact snapshot this COP will also
        // bind if the guard passes -- not a fresh read. Per ADR-133, this
        // guard also covers a staged PDU_PC_TESTER_PRESENT-class ComParam
        // change (ISO 22900-2 §9.4.16.2.1 f): ComParams of type
        // PDU_PC_TESTER_PRESENT cannot be altered temporarily via the
        // TempParamUpdate flag) -- closed as a Codex review finding on
        // PR #147.
        if temp_param_update
            && temp_eligible
            && let Some((working, active)) = &bound_comparams
            && (comparam_support::bustype_params_differ(working, active)
                || comparam_support::tester_present_params_differ(working, active))
        {
            return Err(state_guard_status(
                Code::FailedPrecondition,
                "PDU_ERR_TEMPPARAM_NOT_ALLOWED: temp_param_update cannot stage a \
                 PDU_PC_BUSTYPE- or PDU_PC_TESTER_PRESENT-class ComParam change \
                 (Working differs from Active on a bus-physical or tester-present \
                 param); issue CoptUpdateparam to promote it instead",
                PduError::PduErrTempparamNotAllowed,
                last_error_at_snapshot,
            ));
        }

        // SendRecv/StartComm/non-empty-data StopComm actively transmit on the
        // physical bus, so their TX items are subject to another CLL's held
        // `LOCK_PHYSICAL_TX_QUEUE` -- but as of ADR-123 this is no longer a
        // synchronous call-time reject here. The COP is always accepted and
        // enqueued; `dispatch_tx_item`'s TX-suspend siphon (`LogicalLinkState
        // ::tx_suspended()`) holds it in `tx_held` for as long as the lock is
        // held, and `recompute_lock_tx_suspensions` (triggered by the
        // eventual `UnlockResource`/disconnect/destroy that releases the
        // lock) resumes it -- mirroring ISO 22900-2 §9.4.13.3 use case 1
        // (a newly created ComLogicalLink starts with its ComPrimitive queue
        // suspended) rather than rejecting outright.

        // ADR-123 Fix H: NumSendCycles == 0 = receive-only, no bus write (ADR-059)
        // -- same field handle_send_recv's should_transmit reads. Parsed once here
        // and reused as the CoptSendrecv arm's num_send_cycles so the two can't
        // drift into different values for the same call.
        let num_send_cycles = request
            .cop_ctrl_data
            .as_ref()
            .map(|c| c.num_send_cycles)
            .unwrap_or(0);

        // ADR-123: classify once, at StartComPrimitive call time, whether
        // this COP actually transmits on the physical bus -- mirrors
        // TxItem::transmits()'s per-variant classification exactly, since
        // the TxItem built below (per cop_type) is what that method reads.
        let transmits = match cop_type {
            vci_service_interface::ComOperationType::CoptSendrecv => num_send_cycles != 0,
            vci_service_interface::ComOperationType::CoptStartcomm => true,
            vci_service_interface::ComOperationType::CoptStopcomm => !request.cop_data.is_empty(),
            _ => false,
        };

        // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): this is
        // clause 10's own explicit write-disallowed rule (only
        // `PassThruReadMsgs` works on this protocol), not a generic
        // protocol-capability gap -- mirrors the exact rejection pattern
        // `rpc_misc.rs`'s `ioctl_start_repeat_message` uses for UART Echo
        // Byte + Repeat Messaging. Checked here, before `cop_handle`
        // allocation or any state mutation, using `transmits`'s own
        // classification (a COP with no actual TX data, e.g. a receive-only
        // `CoptSendrecv` with `num_send_cycles == 0`, is unaffected).
        if transmits && resources::is_analog_in_protocol_id(link.hw_protocol_id) {
            return Err(state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_ID_NOT_SUPPORTED: writing (CoptSendrecv/CoptStartcomm/a non-empty \
                 CoptStopcomm cop_data) is not supported on a SAE J2534-2 clause 10 Analog Input \
                 link -- clause 10 defines this protocol as read-only; only PassThruReadMsgs is \
                 valid",
                PduError::PduErrIdNotSupported,
                link.last_error,
            ));
        }

        // ADR-180 Decision 14 (design-advisor consult, PR #72 round 12,
        // Finding 1 Part A): a transmitting `CoptSendrecv` (`transmits`,
        // this arm's own `num_send_cycles != 0` classification -- a
        // receive-only monitor per ADR-059 puts nothing on the bus and has
        // nothing to gate) on a negotiation-enabled SAE J1939 CLL with no
        // claimed source address yet (`j1939_negotiated_unclaimed_at_bind`,
        // captured above alongside `bound_comparams`) is rejected
        // synchronously here, before `cop_handle` allocation -- mirroring
        // the analog-input rejection just above and ADR-180 Decision 13's
        // own Repeat-Messaging gate's identical rationale: SAE J1939's own
        // claim-before-transmit precondition (clause 16), and this
        // codebase's `ERR_ADDRESS_NOT_CLAIMED` precedent elsewhere in this
        // file. This is the enqueue-time half of Decision 14's two-part fix
        // -- `events.rs::handle_send_recv`'s per-cycle re-check (via
        // `SendRecvTx::j1939_tx_source`) is the transmit-time half that
        // closes the TOCTOU this gate alone cannot (a claim spontaneously
        // lost, or still pending, between this call and a later cycle's
        // actual dispatch).
        if cop_type == vci_service_interface::ComOperationType::CoptSendrecv
            && transmits
            && j1939_negotiated_unclaimed_at_bind
        {
            return Err(state_guard_status(
                Code::FailedPrecondition,
                "this SAE J1939 ComLogicalLink has no claimed source address yet \
                 (CP_J1939AddressNegotiationRule requests negotiation, ADR-179 Decision 3) -- \
                 issue and complete a successful CoptStartcomm, or wait for a spontaneous \
                 reclaim to finish, before sending on it",
                PduError::PduErrCllNotStarted,
                link.last_error,
            ));
        }

        let cop_handle = self.next_primitive_handle().await;
        self.primitives.lock().await.insert(
            cop_handle,
            CopEntry {
                cll_handle: handle,
                dispatched: false,
                transmits,
                is_send_recv: cop_type == vci_service_interface::ComOperationType::CoptSendrecv,
                // ADR-204: the client's own correlation tag, if any --
                // validated against MAX_COP_TAG_LEN above. Echoed on every
                // COP-status event this cop_handle produces while this
                // entry is live (events::send_cop_status).
                cop_tag: request.cop_tag.clone(),
            },
        );

        // Retrieve the tx_queue sender once; it is None only if the link is not
        // connected, which is guarded above.
        let channel_key = {
            let links = self.logical_links.lock().await;
            links
                .get(&handle)
                .and_then(|l| l.channel_key)
                .ok_or_else(|| Status::internal("link is connected but channel_key is not set"))?
        };
        let tx_queue = {
            let chans = self.shared_channels.lock().await;
            chans
                .get(&channel_key)
                .map(|sc| sc.tx_queue.clone())
                .ok_or_else(|| Status::internal("shared channel not found for connected link"))?
        };

        match cop_type {
            vci_service_interface::ComOperationType::CoptSendrecv => {
                let ctrl = request.cop_ctrl_data.as_ref();
                let tx_flags = ctrl
                    .map(|c| compute_j2534_tx_flags(c, link.raw_mode, link.hw_protocol_id))
                    .unwrap_or(0);
                // ADR-068: the Active UniqueRespIdTable snapshot already
                // bound, atomically alongside `bound_comparams`, above
                // (`bound_active_table`) -- not re-read here.
                let entries = bound_active_table;

                // ComParam-independent: this buffer is always exactly
                // [4-byte CAN ID][payload] in software ISO-TP mode
                // regardless of addressing (tx_header::can_header_bytes), so
                // "non-empty" depends only on cop_data.len() and the link's
                // (fixed once connected) software_isotp flag.
                if link.software_isotp && request.cop_data.is_empty() {
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::invalid_argument(
                        "cop_data must be a non-empty payload",
                    ));
                }

                // ADR-067 claim A: use the ComParam snapshot already bound,
                // in a single critical section, above (`bound_comparams`) --
                // Working when temp_param_update is set (the poll task
                // reverts hardware to the live Active set afterward), else
                // the Active snapshot (hardware already reflects it, no
                // push/revert needed). Not re-read here.
                let (bound_working, bound_active) = bound_comparams.unwrap_or_default();
                let binding = if temp_param_update {
                    ParamBinding::Temp {
                        effective: bound_working,
                    }
                } else {
                    ParamBinding::Plain(bound_active)
                };

                // SAE J2534-2 clause 19.3.2.2 (ADR-192/Phase 7 Stage 7c):
                // reject an out-of-range `CP_TP20BroadcastAddress` outright,
                // synchronously, before `resolve_send_recv_tx`/
                // `build_tx_message` ever sees it. Fix 4 (Codex review, P2,
                // PR #101): shared with the `CoptStartcomm`/`CoptStopcomm`
                // optional-message paths below via
                // `validate_tp20_broadcast_address_range` -- see that
                // function's own doc comment.
                // ADR-210 Decision item 9: re-keyed from the narrow
                // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id` --
                // `link.hw_protocol_id` is a live link's raw id, so a
                // `_CHx`-connected TP2.0 link would otherwise skip this
                // synchronous rejection entirely.
                let is_tp20_link = resources::is_tp2_0_family_protocol_id(link.hw_protocol_id);
                if is_tp20_link
                    && let Err(err) = validate_tp20_broadcast_address_range(binding.resolved())
                {
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::invalid_argument(err));
                }
                // Resolved (post-range-check) broadcast intent, reused below
                // to reject request shapes ADR-192 Consequences documents as
                // unsupported (a response expectation, or an unsupported
                // finite `num_send_cycles`).
                let is_broadcast_send =
                    is_tp20_link && binding.resolved().tp20_broadcast_address().is_some();

                // ADR-067 claim B: resolution moves back to the RPC handler,
                // synchronously, against the bound snapshot -- reverting
                // ADR-064's deferral to the poll task's first cycle. A
                // resolution failure (missing addressing ComParam, TX size
                // out of range, ISO15765-2 functional Single Frame limit) is
                // once again a synchronous INVALID_ARGUMENT, as it was before
                // ADR-064.
                let resolved = match resolve_send_recv_tx(
                    link.protocol,
                    link.hw_protocol_id,
                    binding.resolved(),
                    &entries,
                    &request.cop_data,
                    link.software_isotp,
                    tx_flags,
                    tp20_established_tx_id,
                    link.raw_mode,
                    link.checksum_mode,
                ) {
                    Ok(resolved) => resolved,
                    Err(err) => {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(err));
                    }
                };
                // ADR-196 Decision item 3b: the ONE shared helper computing
                // this send's raw CAN-ID prefix width, from the RESOLVED
                // native TX flags (not a ComParam re-read) -- reused by
                // `request_sid`'s capture just below and forwarded, via
                // `tx_prefix`, for `TimingChangeConfig::with_request` to
                // re-base its own anchors against wherever this COP's
                // `timing_cfg` is built.
                let tx_prefix = compute_tx_prefix(
                    link.raw_mode,
                    link.hw_protocol_id,
                    resolved.tx_flags,
                    &request.cop_data,
                );
                let tx = SendRecvTx {
                    data: resolved.data,
                    tx_flags: resolved.tx_flags,
                    isotp_tx: resolved.isotp_tx,
                    can_functional: resolved.can_functional,
                    request_sid: request.cop_data.get(tx_prefix).copied(),
                    access_timing_request: (!request.cop_data.is_empty())
                        .then(|| request.cop_data.clone()),
                    j1939_tx_source: resolved.j1939_tx_source,
                    tp20_established_tx_id: resolved.tp20_established_tx_id,
                    tp20_is_broadcast: resolved.tp20_is_broadcast,
                    tx_prefix,
                };

                // Parse expected-response descriptors from ComPrimitiveCtrlData.
                // Fire-and-forget (PduCopstFinished immediately after
                // PassThruWriteMsgs, no receive phase) is driven purely by
                // NumReceiveCycles == 0, not by this list being empty (ADR-058):
                // an empty list with NumReceiveCycles > 0 naturally times out
                // instead, since no frame can ever match.
                let expected_response: Vec<ExpectedResponse> = ctrl
                    .map(|c| {
                        c.expected_response_array
                            .iter()
                            .map(|e| ExpectedResponse {
                                mask: e.mask_data.clone(),
                                pattern: e.pattern_data.clone(),
                                unique_resp_ids: e.unique_resp_ids.clone(),
                                acceptance_id: e.acceptance_id,
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                // PDU_COP_CTRL_DATA cycle control (ADR-053, ADR-059).  Time
                // is the cyclic-send cycle time in ms (0 = re-enqueue each
                // follow-up cycle at the back of the TX queue), NOT the
                // response timeout — the response window comes from
                // CP_P2Max in the bound ParamBinding (ADR-067). NumSendCycles: 0 = no
                // send at all (receive-only, a single non-repeating pass),
                // n > 0 = exactly n sends, -1 = infinite.  NumReceiveCycles:
                // 0 = no response required, n > 0 = exact match count, -1
                // IS-CYCLIC, -2 IS-MULTIPLE.  NumSendCycles == 0 and
                // NumReceiveCycles == 0 together mean neither send nor
                // receive happens at all; other state changes this COP would
                // otherwise perform (temp_param_update, status transitions)
                // still occur.
                let cycle_time_ms = ctrl.map(|c| c.time).unwrap_or(0);
                // num_send_cycles is computed once, above, before the
                // transmits classification -- not re-parsed here.
                let num_receive_cycles = ctrl.map(|c| c.num_receive_cycles).unwrap_or(0);
                if num_send_cycles < -1 {
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::invalid_argument(format!(
                        "num_send_cycles must be >= -1 (-1 = infinite cyclic send), got {num_send_cycles}"
                    )));
                }
                if num_receive_cycles < -2 {
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::invalid_argument(format!(
                        "num_receive_cycles must be >= -2 (-1 = IS-CYCLIC, -2 = IS-MULTIPLE), got {num_receive_cycles}"
                    )));
                }
                // SAE J2534-2 clause 19.3.2.2/19.3.2.3 (ADR-192/Phase 7 Stage
                // 7c Consequences): no response is attributable to a
                // broadcast send under this service's per-connection
                // response routing, so a broadcast CoptSendrecv asking to
                // wait for one is rejected outright, not degraded to a
                // response-less send. `num_send_cycles` outside `{1, -1}` is
                // also rejected: the native adapter offers only "burst once"
                // (`PassThruWriteMsgs`, `num_send_cycles == 1`) and "burst
                // once then repeat forever at the configured rate"
                // (`PassThruStartPeriodicMsg`, `num_send_cycles == -1`) --
                // no native primitive exists for a finite repeat count
                // greater than the fixed five-frame burst.
                if is_broadcast_send {
                    if num_receive_cycles != 0 {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(format!(
                            "a TP2.0 broadcast CoptSendrecv (CP_TP20BroadcastAddress set) must \
                             have num_receive_cycles == 0 -- no response is attributable to a \
                             broadcast send, got {num_receive_cycles}"
                        )));
                    }
                    if ctrl.is_some_and(|c| !c.expected_response_array.is_empty()) {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(
                            "a TP2.0 broadcast CoptSendrecv (CP_TP20BroadcastAddress set) must \
                             not carry an expected_response_array -- no response is attributable \
                             to a broadcast send",
                        ));
                    }
                    if num_send_cycles != 1 && num_send_cycles != -1 {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(format!(
                            "a TP2.0 broadcast CoptSendrecv (CP_TP20BroadcastAddress set) only \
                             supports num_send_cycles == 1 (single five-frame burst) or -1 \
                             (periodic re-trigger) -- no native primitive exists for any other \
                             finite repeat count, got {num_send_cycles}"
                        )));
                    }
                }
                let send_cycles_remaining = num_send_cycles;

                // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c):
                // a cyclic broadcast (`is_broadcast_send`, `num_send_cycles
                // == -1`, enforced above) bypasses the ordinary tx_queue/
                // poll-task dispatch pipeline entirely and issues a real
                // native `PassThruStartPeriodicMsg` directly, here -- that
                // pipeline calls `PassThruWriteMsgs` once per configured
                // period, which would re-emit the fixed five-frame burst on
                // every tick instead of a single alternating frame after the
                // initial burst (ADR-192 Decision item 2). A single-shot
                // broadcast burst (`num_send_cycles == 1`) needs no such
                // special-casing -- the ordinary `PassThruWriteMsgs` path
                // below already produces the correct 5x burst, since the
                // native device does that unconditionally whenever
                // `TX_FLAG_TP2_0_BROADCAST_MSG` is set with a valid
                // `Data[0]` (already composed by `build_tx_message`/ORed in
                // by `apply_resolved_tx_flags` above).
                if is_broadcast_send && num_send_cycles == -1 {
                    // ISO 22900-2 §9.4.13.3 use case 1 / ADR-123: this branch
                    // issues a real native `PassThruStartPeriodicMsg` call
                    // directly, below, bypassing the ordinary tx_queue/
                    // `dispatch_tx_item` pipeline entirely (this function's
                    // own comment just above) -- which is also where every
                    // OTHER transmitting item's `tx_suspended()` siphon check
                    // (client `PDU_IOCTL_SUSPEND_TX_QUEUE`, a sibling CLL's
                    // held `LOCK_PHYSICAL_TX_QUEUE`, or `CP_SuspendQueueOnError`)
                    // lives. Without an equivalent check here, a broadcast
                    // periodic start would put real traffic on the wire while
                    // a sibling CLL believes it holds exclusive transmit
                    // privilege over this same physical resource -- exactly
                    // the guarantee `LOCK_PHYSICAL_TX_QUEUE` exists to give.
                    // Unlike an ordinary `TxItem`, a native periodic-message
                    // start has no `tx_held`-shaped "queue now, dispatch once
                    // unblocked" representation to defer into, so this
                    // rejects synchronously instead (an ordinary item's own
                    // ComPrimitive is accepted and queued; this one is not
                    // accepted at all).
                    //
                    // Only one live broadcast-periodic message per CLL is
                    // supported -- `tp20_broadcast_periodic` tracks a single
                    // `Tp20BroadcastPeriodic`, so a second concurrent start
                    // would otherwise silently clobber the first one's
                    // tracking `Option`: the first COP's native periodic
                    // message becomes permanently unreachable (nothing keys
                    // off it anymore -- `CoptCancel`, CLL teardown, and
                    // `CLEAR_PERIODIC_MSGS` all read `tp20_broadcast_periodic`
                    // to find the message to stop) and the first `cop_handle`
                    // is orphaned in `self.primitives` forever.
                    //
                    // Fix 2 (Codex review, P1, PR #101): the suspension
                    // check, the already-active check, AND the reservation
                    // write all happen in the SAME critical section (one
                    // `logical_links` lock acquisition, `reserve_tp20_
                    // broadcast_periodic` below), not several separate ones
                    // -- a version that checked-then-released before the
                    // native `start_periodic_message` await below would let
                    // two concurrent `StartComPrimitive` calls both observe
                    // "not active" and "not suspended", both proceed to
                    // start a real native periodic message, and have
                    // whichever write lands second silently clobber the
                    // first's tracking `Option`, via the exact TOCTOU race
                    // this whole check exists to close (just widened from a
                    // single-threaded ordering bug to a race window).
                    //
                    // Fix 3 (Codex review, P1, PR #101): the SAME critical
                    // section also re-verifies this CLL's `connect_generation`/
                    // `connected` against the `LinkView` snapshot (`link`)
                    // captured much earlier in this call (`get_link_state`,
                    // before this TP2.0-specific branch) -- a disconnect
                    // (and possibly reconnect, even onto a different physical
                    // channel) landing in the gap between that snapshot and
                    // here would otherwise let a stale `channel_id`/
                    // `hw_protocol_id` reach the native call below, targeting
                    // a channel this CLL may no longer actually be connected
                    // to. `channel_id`/`hw_protocol_id` are therefore read
                    // from the LIVE `LogicalLinkState`, in this SAME critical
                    // section, never from the stale `link` snapshot -- see
                    // `reserve_tp20_broadcast_periodic`'s own doc comment.
                    //
                    // `Tp20BroadcastPeriodic { message_id: None, .. }`
                    // reserves the slot: `None` is an out-of-band sentinel
                    // (never a value `start_periodic_message` can return, in
                    // contrast to an in-band magic number like `0`, which SAE
                    // J2534-1 clause 7.2.7.2's unconstrained `pMsgID`
                    // assignment does not rule out a conformant adapter
                    // legitimately returning -- Codex review, PR #101) --
                    // overwritten with `Some(id)` below on success, rolled
                    // back to `None` on failure. Every other site reading
                    // `tp20_broadcast_periodic` treats `message_id.is_none()`
                    // as "a start is in flight, nothing real to stop yet"
                    // (see each site's own comment).
                    //
                    // Fix 2: every early return from here through the native
                    // call below must roll back this cop_handle's own
                    // `None`-sentinel reservation (via
                    // `rollback_tp20_broadcast_periodic_reservation`, which
                    // also removes the `primitives` entry) instead of the
                    // plain `self.primitives.lock().await.remove(&cop_handle)`
                    // every OTHER failure path in this match arm uses -- the
                    // reservation was written above, before this point, so a
                    // bare `primitives` removal alone would leave it
                    // permanently blocking this CLL from ever starting a
                    // real broadcast periodic message.
                    let (channel_id, hw_protocol_id) = match self
                        .reserve_tp20_broadcast_periodic(
                            handle,
                            cop_handle,
                            link.connect_generation,
                        )
                        .await
                    {
                        Ok(pair) => pair,
                        Err(status) => {
                            self.primitives.lock().await.remove(&cop_handle);
                            return Err(status);
                        }
                    };
                    let mut message = match j2534_0404::PassThruMessage::new(
                        hw_protocol_id,
                        0,
                        tx.tx_flags,
                        0,
                        0,
                        &tx.data,
                    ) {
                        Ok(message) => message,
                        Err(err) => {
                            self.rollback_tp20_broadcast_periodic_reservation(handle, cop_handle)
                                .await;
                            return Err(Status::invalid_argument(format!(
                                "failed to construct the TP2.0 broadcast periodic message: {err}"
                            )));
                        }
                    };
                    // Codex review round 8 Finding 1 (PR #101, ADR-192/Phase
                    // 7 Stage 7c): `self.api` is acquired ONCE, here, and
                    // held CONTINUOUSLY across the whole apply -> native
                    // `start_periodic_message` -> revert bracket below --
                    // not three separate `self.api.lock().await`
                    // acquisitions with real gaps between them, which is
                    // what this used to be (Fix 5 / round-5's own
                    // "periodic-clear epoch" fix / round 8's own apply-
                    // failure-revert fix, each landing its own separate
                    // lock/unlock). A gap there let a concurrent operation
                    // on the same physical channel -- a sibling CLL's own
                    // temp-bound broadcast start, or a queued
                    // `CoptUpdateparam` -- interleave and corrupt which
                    // hardware value this burst actually transmits with.
                    // Every exit path below (apply failure, native-start
                    // failure, native-start success) explicitly `drop(api)`
                    // before touching `self.logical_links` for its own
                    // `last_error` read, calling
                    // `rollback_tp20_broadcast_periodic_reservation`, or
                    // returning `Err` -- and, on the success path, before
                    // `finalize_broadcast_periodic_start_bookkeeping`, whose
                    // status emissions need no fence (round 15's split,
                    // ADR-193, deliberately keeps only the resolution half,
                    // `finalize_or_orphan_broadcast_periodic_start_locked`,
                    // inside this guard). Reading `self.logical_links` (for `last_error`,
                    // or inside `revert_hardware_to_live_active_locked`)
                    // WHILE this `api` guard is still held is fine and
                    // sanctioned (ADR-110 amendment: `api` outer,
                    // `logical_links` inner -- `handle_update_param`'s own
                    // `api`-held `logical_links` read is the established
                    // precedent) -- only the reverse order (acquiring `api`
                    // while already holding `logical_links`) is forbidden;
                    // this bracket never does that.
                    let api = self.api.lock().await;

                    // Codex review round 15 Fix 2 (P1, PR #101, ADR-193
                    // partially superseding ADR-192 Decision item 2's
                    // in-flight-reservation mechanism): re-run
                    // `reserve_tp20_broadcast_periodic`'s full predicate set
                    // NOW, under the `api` guard just acquired and strictly
                    // before the temp-param apply and the native start below.
                    // The reservation was written under `logical_links`
                    // alone, and a terminator needs nothing but that same
                    // lock to take it -- so between the reservation and this
                    // point, a `CoptCancel`/TX-suspension termination/CLL
                    // teardown could take it and report this COP terminal
                    // while this call was still queued behind `api`, after
                    // which `PassThruStartPeriodicMsg` would still run and
                    // emit clause 19.3.2.3's five-frame burst synchronously,
                    // with no later best-effort stop able to retract it. See
                    // `revalidate_tp20_broadcast_periodic_reservation`'s own
                    // doc comment for the whole `api`-as-fence design.
                    match self
                        .revalidate_tp20_broadcast_periodic_reservation(
                            &api,
                            handle,
                            cop_handle,
                            link.connect_generation,
                        )
                        .await
                    {
                        BroadcastPeriodicRevalidation::Ok => {}
                        BroadcastPeriodicRevalidation::Rejected(status) => {
                            drop(api);
                            // The entry is still nominally THIS cop_handle's
                            // own `None`-sentinel (that is what distinguishes
                            // `Rejected` from `AlreadyResolved`, see
                            // `BroadcastPeriodicRevalidation`'s own doc
                            // comment), so nobody else has taken over the
                            // cleanup duty: this call must clear both the
                            // reservation and its own `primitives` entry
                            // itself, exactly as every other failure path in
                            // this branch does.
                            self.rollback_tp20_broadcast_periodic_reservation(handle, cop_handle)
                                .await;
                            return Err(*status);
                        }
                        BroadcastPeriodicRevalidation::AlreadyResolved => {
                            drop(api);
                            // The reservation is simply gone: some OTHER path
                            // (a `CoptCancel`, a TX-suspension termination, a
                            // CLL teardown) already TOOK it, and with it the
                            // sole responsibility for eventually finalizing
                            // this cop_handle -- so this call touches NOTHING
                            // here. In particular it must NOT call
                            // `rollback_tp20_broadcast_periodic_reservation`
                            // (edge-case-hunter finding, round 15 follow-up,
                            // ADR-193): that helper also removes the
                            // `primitives` entry, and every terminator's own
                            // finalization is gated on that entry still being
                            // present (`events::emit_terminal_if_live`, or
                            // `CoptCancel`'s own `prims.remove(..).is_some()`
                            // guard). A terminator that takes the sentinel
                            // under `logical_links` alone and only then
                            // queues for `self.api` (`ioctl_suspend_tx_queue`
                            // -> `terminate_tp20_broadcast_periodic_for_suspension`),
                            // or that has already dropped `self.api` but not
                            // yet reached its own `primitives` removal
                            // (`CoptCancel`, `DisconnectComLogicalLink`), has
                            // not reported anything YET -- removing its
                            // `primitives` entry from under it made the COP
                            // vanish with no terminal status and no
                            // `terminal_cops` record at all, so a later
                            // `GetStatus`/`CancelComPrimitive` wrongly saw
                            // `PDU_ERR_INVALID_HANDLE` instead of ADR-128's
                            // already-terminal no-op. There is nothing to
                            // clean up in `logical_links` either -- the entry
                            // being gone is what `AlreadyResolved` means.
                            //
                            // This mirrors
                            // `finalize_broadcast_periodic_start_bookkeeping`'s
                            // `Resolution::NotOwned` arm exactly (which also
                            // does nothing, for the same reason) one step
                            // later in the same bracket, and like that arm the
                            // RPC still reports success without emitting any
                            // status of its own -- INCLUDING ADR-067 claim D's
                            // `temp_param_update` Working-writeback, performed
                            // right here because this arm returns early and so
                            // never reaches this function's own `Ok` tail
                            // (edge-case-hunter finding, round 15 follow-up,
                            // ADR-193 Consequences).
                            //
                            // An earlier revision skipped it, reasoning that
                            // this exit is strictly before the temp-bound
                            // Working snapshot is applied to hardware so
                            // "nothing consumed it." That reasoning is wrong
                            // twice over. Claim D's criterion is acceptance,
                            // not hardware consumption -- `CoptStopcomm`
                            // touches no hardware and still writes back (see
                            // the tail's own comment) -- and the invariant the
                            // tail states is that the writeback is skipped
                            // only for a REJECTED call. This arm is not a
                            // rejection: the `primitives` entry was already
                            // created and handed to the terminator, and the
                            // RPC returns `Ok` with this cop_handle. Skipping
                            // the writeback left the client's staged Working
                            // values in place after a call it was told
                            // succeeded, so a later `CoptUpdateparam` would
                            // promote values that a nanosecond's different
                            // race timing would have discarded.
                            self.apply_temp_param_working_writeback(
                                handle,
                                temp_param_update && temp_eligible,
                            )
                            .await;
                            return Ok(Response::new(
                                vci_service_interface::ComPrimitiveResponse {
                                    cop_handle: Some(vci_service_interface::ComPrimitiveHandle {
                                        module_handle: DEFAULT_MODULE_HANDLE,
                                        cll_handle: handle,
                                        cop_handle,
                                    }),
                                },
                            ));
                        }
                    }

                    // Round 18 (Codex review, P2, PR #101, ADR-192 Decision
                    // item 3 amendment), scoped to `ParamBinding::Temp` only
                    // (edge-case-hunter adversarial review, PR #101, minor
                    // Finding 6 -- the capture below used to run
                    // unconditionally here, including for a `Plain`-bound
                    // broadcast periodic, which never applies a temp value
                    // and never needs a channel-wide restore at all; that
                    // issued a spurious native `GET_CONFIG` call whose
                    // result was entirely discarded, plus a spurious
                    // `warn!` on any adapter that rejects the read --
                    // `events.rs`'s `handle_send_recv` already scopes this
                    // capture inside its own `ParamBinding::Temp` arm, so
                    // this mirrors that): defaults to empty for a `Plain`
                    // binding, matching every revert call site in this
                    // bracket, which is already gated on `ParamBinding::
                    // Temp` and so never actually consumes this value in
                    // that case anyway.
                    let mut channel_wide_restore: Vec<(u32, u32)> = Vec::new();

                    // Fix 5 (Codex review, P2, PR #101, ADR-192/Phase 7
                    // Stage 7c): a Temp-bound COP's Working snapshot is
                    // temp-applied to hardware for exactly this one native
                    // `start_periodic_message` call, then reverted to the
                    // live Active set immediately after -- mirroring
                    // `handle_send_recv`'s own "borrow Working for one TX
                    // then revert" bracket (ISO 22900-2 §9.4.3) exactly, per
                    // `ParamBinding::Temp`'s own doc comment ("finally-style,
                    // on every path including a failed hardware apply and a
                    // failed TX/init"). A native periodic-message start is
                    // itself exactly ONE service-initiated action, even
                    // though its effects continue autonomously device-side
                    // afterward -- so this is architecturally the SAME shape
                    // as an ordinary temp-scoped dispatch, not a new "revert
                    // at COP-end" design; reverting only once the periodic
                    // message is later stopped is deliberately NOT what this
                    // does. Only applies when `binding` is `ParamBinding::
                    // Temp` -- a `Plain` binding already has hardware
                    // reflecting Active, matching `handle_send_recv`'s own
                    // `match &binding` shape exactly.
                    if let ParamBinding::Temp { effective } = &binding {
                        // Round 18 (Codex review, P2, PR #101, ADR-192
                        // Decision item 3 amendment): capture the channel's
                        // real pre-bracket hardware value for
                        // `CHANNEL_WIDE_UNUM32` keys (currently just
                        // `CP_TP20BroadcastInterval`) NOW, still under the
                        // `api` guard just acquired above and strictly
                        // BEFORE the temp-param apply below -- so nothing
                        // can interleave and change the channel's value
                        // between this capture and the apply. Threaded
                        // through to every revert call site in this
                        // bracket, restoring this captured value instead of
                        // this CLL's own per-CLL `Active` (which could be
                        // stale relative to a sibling CLL sharing this
                        // physical channel) -- see `events::
                        // capture_channel_wide_hardware_locked`'s own doc
                        // comment for the full sibling-CLL-clobber fix this
                        // closes.
                        channel_wide_restore = events::capture_channel_wide_hardware_locked(
                            &api,
                            channel_id,
                            hw_protocol_id,
                        )
                        .await;

                        let effective = comparam_support::strip_bustype_keys(effective);
                        let applied = events::apply_params_to_hardware_locked(
                            &api,
                            channel_id,
                            hw_protocol_id,
                            &effective,
                        )
                        .await;
                        if !applied {
                            // Codex review fix (P2, PR #101, round 8):
                            // `apply_params_to_hardware_locked` reporting
                            // `false` does not mean hardware is untouched --
                            // a multi-key `SET_CONFIG` batch can partially
                            // apply before failing, exactly as
                            // `handle_send_recv`'s own `temp_params_ok`
                            // handling already documents ("the ADR-067
                            // hardware-cleanup obligation... has to run
                            // regardless of staleness"). Revert to live
                            // Active (still under this same `api` guard --
                            // see `revert_hardware_to_live_active_locked`'s
                            // own doc comment on the "`active` is a LIVE
                            // read, never pre-fetched" contract, ADR-067)
                            // before dropping `api` and rolling back the
                            // reservation, mirroring the native-start-
                            // failure branch just below (search "Fix 5" in
                            // this same file) -- otherwise a failed apply
                            // here could leave the channel running
                            // Working-only settings indefinitely, with no
                            // periodic message ever started to trigger the
                            // ordinary revert. Accepted residual
                            // (edge-case-hunter, PR #101 round 8
                            // verification): this branch has no dedicated
                            // test, unlike the native-start-failure branch
                            // just below it -- the mock has no hook to force
                            // a selective `SET_CONFIG` failure at exactly
                            // this point (same accepted-residual class
                            // `docs/implementation-notes.md` already documents
                            // for the TP2.0 passive-listener arm's own
                            // second-call-failure rollback path). Logically
                            // correct by inspection and structurally
                            // identical to the covered branch below.
                            events::revert_hardware_to_live_active_locked(
                                &api,
                                &self.logical_links,
                                handle,
                                channel_id,
                                hw_protocol_id,
                                &channel_wide_restore,
                            )
                            .await;
                            drop(api);
                            let last_error = self
                                .logical_links
                                .lock()
                                .await
                                .get(&handle)
                                .and_then(|l| l.last_error.clone());
                            self.rollback_tp20_broadcast_periodic_reservation(handle, cop_handle)
                                .await;
                            return Err(state_guard_status(
                                Code::FailedPrecondition,
                                "PDU_ERR_FCT_FAILED: failed to apply the temp-bound Working \
                                 ComParam snapshot to hardware before starting the TP2.0 \
                                 broadcast periodic message (temp_param_update=1) -- the \
                                 periodic message was not started",
                                PduError::PduErrFctFailed,
                                last_error,
                            ));
                        }
                    }
                    // Fix (Codex review round 5, PR #101, ADR-192/Phase 7
                    // Stage 7c "periodic-clear epoch" fix): the
                    // `periodic_clear_epoch` read stays under the SAME `api`
                    // guard as the native call immediately above it -- the
                    // shared `self.api` mutex is what makes reading the
                    // epoch here, versus `CLEAR_PERIODIC_MSGS`'s own
                    // read/bump under the same lock, encode which native
                    // call happened first (see `periodic_clear_epoch`'s own
                    // doc comment on `J2534Service`). Round 8 Finding 1
                    // widened this to the WHOLE apply/start/revert bracket
                    // sharing one guard, rather than just this native call +
                    // epoch read pair having their own.
                    let start_result =
                        match api.start_periodic_message(channel_id, &mut message, cycle_time_ms) {
                            Ok(id) => Ok((
                                id,
                                self.periodic_clear_epoch
                                    .load(portable_atomic::Ordering::Relaxed),
                            )),
                            Err(err) => Err(err),
                        };
                    let (message_id, started_epoch) = match start_result {
                        Ok(pair) => pair,
                        Err(err) => {
                            // Fix 5: a failed native start must not leave
                            // hardware sitting on the borrowed Working
                            // snapshot -- revert (still under this same
                            // `api` guard) before dropping it and rolling
                            // back the reservation, mirroring
                            // `ParamBinding::Temp`'s own "finally-style"
                            // contract.
                            if matches!(&binding, ParamBinding::Temp { .. }) {
                                events::revert_hardware_to_live_active_locked(
                                    &api,
                                    &self.logical_links,
                                    handle,
                                    channel_id,
                                    hw_protocol_id,
                                    &channel_wide_restore,
                                )
                                .await;
                            }
                            drop(api);
                            let last_error = self
                                .logical_links
                                .lock()
                                .await
                                .get(&handle)
                                .and_then(|l| l.last_error.clone());
                            self.rollback_tp20_broadcast_periodic_reservation(handle, cop_handle)
                                .await;
                            return Err(map_native_error_for_link(
                                "PassThruStartPeriodicMsg",
                                &err,
                                last_error,
                            ));
                        }
                    };
                    if matches!(&binding, ParamBinding::Temp { .. }) {
                        events::revert_hardware_to_live_active_locked(
                            &api,
                            &self.logical_links,
                            handle,
                            channel_id,
                            hw_protocol_id,
                            &channel_wide_restore,
                        )
                        .await;
                    }
                    // Track the started periodic message against the owning
                    // COP/CLL -- stopped by `CoptCancel` of this COP,
                    // Disconnect/DestroyComLogicalLink of this CLL
                    // (including the shared-channel case, ADR-192
                    // Consequences), or `rpc_misc.rs`'s
                    // `CLEAR_PERIODIC_MSGS` reconciliation. Extracted into
                    // its own method (Codex review fix, P2, PR #101) so the
                    // reservation-lost/status-suppression logic is directly
                    // unit-testable, mirroring `reserve_tp20_broadcast_periodic`'s
                    // own extraction just above.
                    //
                    // Codex review round 15 Fix 2 (P1, PR #101, ADR-193):
                    // the RESOLUTION half runs while this bracket's `api`
                    // guard is STILL held, immediately after the native
                    // start returned -- committing the real `message_id`
                    // (or orphan-stopping a message whose reservation was
                    // taken meanwhile) before any terminator can win the
                    // fence and observe a half-resolved state. Only the
                    // bookkeeping half, which needs no `api`, runs after
                    // `drop(api)` below.
                    let resolution = self
                        .finalize_or_orphan_broadcast_periodic_start_locked(
                            &api,
                            handle,
                            cop_handle,
                            channel_id,
                            message_id,
                            started_epoch,
                        )
                        .await;
                    drop(api);
                    self.finalize_broadcast_periodic_start_bookkeeping(
                        handle, cop_handle, message_id, resolution,
                    )
                    .await;
                } else if tx_queue
                    .send(TxItem::SendRecv {
                        cop_handle,
                        cll_handle: handle,
                        protocol_id: link.hw_protocol_id,
                        logical_protocol: link.protocol,
                        base_protocol_id: link.base_hw_protocol_id(),
                        tx,
                        binding,
                        expected_response,
                        cycle_time_ms,
                        send_cycles_remaining,
                        num_receive_cycles,
                        connect_generation,
                    })
                    .is_err()
                {
                    // Poll task has exited (receiver dropped).  Remove the entry we
                    // just inserted so GetStatus does not permanently return Executing.
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::internal("tx queue closed unexpectedly"));
                }
                // PduCopstExecuting and PduCopstFinished are both emitted by the
                // poll task: Executing before the first cycle's PassThruWriteMsgs,
                // Finished after the LAST send cycle completes its receive phase
                // (num_receive_cycles matches, or the CP_P2Max window elapsing).
                // Infinite operation (num_send_cycles / num_receive_cycles = -1)
                // ends only via CancelComPrimitive or disconnect (ADR-053).
            }

            vci_service_interface::ComOperationType::CoptStartcomm => {
                // Re-check comm_started under the lock to close the TOCTOU window
                // between the pre-flight snapshot (get_link_state above) and here.
                {
                    let links = self.logical_links.lock().await;
                    if links.get(&handle).map(|l| l.comm_started).unwrap_or(false) {
                        let last_error = links.get(&handle).and_then(|l| l.last_error.clone());
                        drop(links);
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(state_guard_status(
                            Code::FailedPrecondition,
                            "comm is already started; issue CoptStopcomm before starting again",
                            PduError::PduErrCllConnected,
                            last_error,
                        ));
                    }
                }

                let ctrl = request.cop_ctrl_data.as_ref();
                let base_tx_flags = ctrl
                    .map(|c| compute_j2534_tx_flags(c, link.raw_mode, link.hw_protocol_id))
                    .unwrap_or(0);

                // UniqueRespIdTable snapshot, consumed synchronously below --
                // resolution no longer happens at execution time, so this is
                // not carried in the TxItem (ADR-067). The Active table
                // already bound, atomically alongside `bound_comparams`,
                // above (`bound_active_table`) -- not re-read here (ADR-068).
                let entries = bound_active_table;

                // ADR-067 claim A/C: use the ComParam snapshot already
                // bound, in a single critical section, above
                // (`bound_comparams`) -- not re-read here.
                let (bound_working, bound_active) = bound_comparams.unwrap_or_default();
                let binding = if temp_param_update {
                    ParamBinding::Temp {
                        effective: bound_working,
                    }
                } else {
                    ParamBinding::Plain(bound_active.clone())
                };

                // ADR-067 claim C / "claim 8": the periodic tester-present is
                // ALWAYS resolved from the call-time Active snapshot, never
                // Working, even when temp_param_update is set -- it is a
                // persistent product of this COP that outlives the transient
                // init transaction. A resolution failure (e.g. a missing
                // addressing ComParam for a non-empty tester-present payload)
                // is a synchronous INVALID_ARGUMENT again (reverting
                // ADR-066's deferral to an execution-time error event).
                let tester_present = match resolve_tester_present(
                    link.protocol,
                    link.hw_protocol_id,
                    &bound_active,
                    &entries,
                    link.software_isotp,
                    base_tx_flags,
                    link.raw_mode,
                ) {
                    Ok(resolved) => resolved,
                    Err(err) => {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(err));
                    }
                };

                // Init transaction TxFlags: from the bound binding (Working
                // when temp_param_update is set, else Active) -- scoped to
                // the init step only, unlike tester_present above.
                let init_tx_flags = resolve_init_tx_flags(
                    binding.resolved(),
                    &entries,
                    base_tx_flags,
                    link.hw_protocol_id,
                    link.raw_mode,
                );

                // 5-baud initialization resolution (ADR-076): both the
                // spec-mandated contract (`CP_InitializationSettings == 1` on
                // a K-line link) and the pre-ADR-076 legacy heuristic path
                // resolve their target address / keybyte-delivery decision
                // here, eagerly, from `binding.resolved()` -- never re-derived
                // by the poll task (mirrors `init_tx_flags`/`fast_init`'s
                // own call-time resolution). See `FiveBaudInit`'s doc comment
                // for the two paths this populates from. `is_kline` mirrors
                // the `fast_init` gate below; `call_time_sequence`
                // reuses `select_init_sequence` (rather than re-deriving the
                // legacy heuristic here) so the "param absent" and
                // defensive "out-of-range value" fallback (ADR-074) cases
                // both resolve identically to that function's own decision.
                // ADR-157 Plane B: normalized -- found during this fix's
                // independent sweep. `is_kline` gates every K-line-only
                // decision in this function (5-baud/fast-init resolution
                // below), so without this an `ISO9141_PS`/`ISO14230_PS` link
                // would never reach `select_init_sequence` at all, making
                // that call's own argument normalization (ADR-157's
                // explicitly enumerated site) moot.
                //
                // ADR-170/Phase 9 (edge-case-hunter finding): SAE J2534-2
                // clause 12 UART Echo Byte Protocol is K-line-physical-layer
                // too and needs the same FIVE_BAUD_INIT/FAST_INIT eligibility
                // this variable gates -- `UART_ECHO_BYTE_PS` self-maps
                // through `base_hw_protocol_id()` (no `hw_protocol_override`,
                // ADR-170 Decision 1), so it needed its own arm here rather
                // than falling out of the existing ISO9141/ISO14230 check for
                // free. Broadened `is_kline` in place (not a narrower
                // parallel flag) after auditing every other consultation of
                // this variable in this function: the `tx` resolution below
                // (~line 1648, "K-line treats cop_data as init-only, never an
                // optional CoptStartcomm message via resolve_send_recv_tx")
                // is the correct behavior for UART Echo Byte too -- clause 12
                // has no optional-message-alongside-init concept either, so
                // `cop_data` must stay init-only for this protocol as well.
                // No ISO9141/ISO14230-specific ComParam or access-timing
                // logic keys off `is_kline` in this function (the only other
                // per-protocol resolution here, `rc_cfg`/`timing_cfg` for the
                // optional CoptStartcomm message, is reached only in the
                // `!is_kline` branch and is therefore unaffected either way).
                // `comparam_support.rs`'s UART Echo Byte allowlist (DATA_RATE/
                // LOOPBACK only) independently keeps every K-line-specific
                // ComParam (`CP_InitializationSettings`,
                // `CP_5BaudAddressPhys`/`Func`, etc.) unsettable for this
                // protocol, so `init_settings`/`five_baud`'s spec-mandated
                // branch (`init_settings == Some(1)`) can never fire for it
                // either -- only the legacy single-cop_data-byte heuristic
                // path (`call_time_sequence == Some(FiveBaud)`, requires
                // `init_data.len() == 1`) can select FIVE_BAUD_INIT for this
                // protocol via this specific arm; `select_init_sequence`'s
                // own `legacy_heuristic` closure DOES also match
                // UART_ECHO_BYTE_PS unconditionally (clause 12.3.2/12.3.4.2:
                // this protocol defines only 5-baud init, no fast-init), as
                // defense-in-depth alongside the synchronous `cop_data.len()
                // != 1` rejection immediately below, which is what actually
                // closes off the empty-data/oversized-data cases for this
                // protocol in practice.
                // ADR-174/Phase 10 (design-advisor decision): SAE J2534-2
                // clause 13 Honda DIAG-H (`PROTOCOL_HONDA_DIAGH_PS`) is
                // deliberately NOT added here, despite being physically
                // K-line-like (UART, single-wire) the same way UART Echo
                // Byte above is -- clause 13.3.1 states the interface
                // performs no initialization process at all for this
                // protocol, so there is no init step for `cop_data` to
                // address, unlike UART Echo Byte's mandatory 5-baud address
                // byte (clause 12, ADR-170 Decision 8) or ISO9141/ISO14230's
                // own init sequences. `is_kline == false` routes a non-empty
                // `CoptStartcomm cop_data` through the ordinary
                // `resolve_send_recv_tx` path below as a genuine optional
                // message, the same treatment CAN/J1850/SCI already get --
                // this is this codebase's existing default for a protocol
                // with no init consumer, not a gap. Do not "fix" this by
                // grouping DIAG-H with UART Echo Byte on physical-layer
                // resemblance alone.
                let is_kline = matches!(
                    link.base_hw_protocol_id(),
                    j2534_0404::ISO9141
                        | j2534_0404::ISO14230
                        | j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
                );

                // SAE J2534-2 clause 16 SAE J1939 (ADR-179 Decision 4): a
                // spec-mandated StartComPrimitive-time gate, separate from
                // the address-claim retry loop itself (`events.rs::handle_
                // start_comm`) -- `CP_J1939TargetAddress == 0xFFFF` (the
                // "not configured" sentinel `comparam_defaults.rs`'s
                // `j1939_can_common` seeds by default) fails the
                // StartComPrimitive outright, synchronously, before
                // `cop_handle` allocation or any enqueue -- mirroring every
                // other synchronous call-time rejection in this function
                // (e.g. the UART Echo Byte cop_data-length check just
                // below). This does not depend on `is_kline`/`tx`
                // resolution below and is checked unconditionally for every
                // J1939 CLL, regardless of whether this StartComm requests
                // an address claim.
                if resources::is_j1939_protocol_id(link.hw_protocol_id) {
                    let j1939_target_address = binding
                        .resolved()
                        .unum32
                        .get(&PARAM_J1939_TARGET_ADDRESS)
                        .copied();
                    if j1939_target_address == Some(0xFFFF) {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(
                            "CP_J1939TargetAddress must be configured (not the 0xFFFF \"not \
                             configured\" sentinel) before a SAE J1939 StartComPrimitive can \
                             succeed",
                        ));
                    }
                    // Codex review finding (PR #72 round 6): `CP_J1939TargetAddress`
                    // is a one-byte wire field (`tx_header::j1939_header_bytes`'s
                    // `target_address as u8` cast) -- an out-of-range value
                    // other than the `0xFFFF` sentinel just rejected above
                    // (e.g. `0x100`) previously passed this gate silently and
                    // was truncated at cast time, addressing a DIFFERENT ECU
                    // than the client's `GetComParam` readback (still
                    // reporting the untruncated value) implies, with no
                    // error surfaced anywhere. Reject synchronously here
                    // instead, mirroring the sentinel check's own shape.
                    if j1939_target_address.is_some_and(|addr| addr > 0xFF) {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(format!(
                            "CP_J1939TargetAddress ({:#06x}) must fit in one byte (0x00-0xFF, \
                             or the 0xFFFF \"not configured\" sentinel) before a SAE J1939 \
                             StartComPrimitive can succeed",
                            j1939_target_address.expect("is_some_and guarantees Some")
                        )));
                    }

                    // Codex review finding (PR #72): `CP_J1939Name` is a
                    // fixed 8-byte Bytefield (clause 16.3.3.2's NAME field) --
                    // `resolve_j1939_claim_params` (`events_j1939_claim.rs`)
                    // silently zero-pads a shorter staged value or truncates
                    // a longer one to fit its `[u8; 8]` native parameter,
                    // rather than rejecting it. That silent transform means
                    // the adapter claims/arbitrates with a DIFFERENT 64-bit
                    // NAME than the client believes it configured (its own
                    // `GetComParam` readback still returns the original,
                    // untransformed bytes) -- the identical "adapter acts on
                    // a value the client's readback disagrees with" failure
                    // mode `CP_J1939TargetAddress`'s own truncation check
                    // above closes for a Unum32 field. An absent/never-
                    // staged value is deliberately NOT rejected here --
                    // `comparam_defaults.rs`'s `j1939_can_common` seeds
                    // `PARAM_J1939_NAME` to an explicit EMPTY Bytefield
                    // (`Some(vec![])`, len 0, not `None`) as its documented
                    // "not configured" default, which `resolve_j1939_claim_
                    // params` zero-pads to an all-zero NAME -- `run_j1939_
                    // claim_loop`'s own dedicated check already fails that
                    // claim closed (clause 16.3.3.2: all-zero NAME is
                    // PROTECT_J1939_ADDR's wire-level CANCEL form, not a
                    // valid claim). Only a NON-EMPTY staged value of the
                    // WRONG length is the new, distinct failure mode this
                    // check closes -- `len == 0` is the documented default,
                    // not an error.
                    let j1939_name_len = binding
                        .resolved()
                        .bytes
                        .get(&PARAM_J1939_NAME)
                        .map(Vec::len);
                    if j1939_name_len.is_some_and(|len| len != 0 && len != 8) {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(format!(
                            "CP_J1939Name ({} bytes) must be exactly 8 bytes (clause \
                             16.3.3.2's NAME field) before a SAE J1939 StartComPrimitive can \
                             succeed",
                            j1939_name_len.expect("is_some_and guarantees Some")
                        )));
                    }
                }

                // ADR-170 Decision 8 (design-advisor fix, closing two
                // edge-case-hunter findings): SAE J2534-2 clause 12 defines
                // no fast-init and no init-less start for this protocol --
                // only 5-baud init exists, and it always needs exactly the
                // one address byte. Rejecting synchronously here (rather
                // than letting an empty `cop_data` fall through to "skip
                // init" or a `>= 4`-byte `cop_data` fall through to the
                // FAST_INIT dispatch below, which clause 12 never defines)
                // closes both findings at the source. Any actual message
                // payload for this protocol belongs in `CoptSendrecv`, not
                // `CoptStartcomm`.
                if link.base_hw_protocol_id() == j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS
                    && request.cop_data.len() != 1
                {
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::invalid_argument(
                        "cop_data must be exactly one byte (the 5-baud init address) for a \
                         PROTOCOL_UART_ECHO_BYTE_PS start communication request -- clause 12 \
                         defines neither fast-init nor an init-less start for this protocol; \
                         message payloads belong in CoptSendrecv, not CoptStartcomm",
                    ));
                }

                let init_settings = binding.resolved().unum32.get(&PARAM_INIT_SETTINGS).copied();
                let call_time_sequence = is_kline.then(|| {
                    super::events::select_init_sequence(
                        binding.resolved(),
                        link.base_hw_protocol_id(),
                        &request.cop_data,
                        handle,
                    )
                });
                let five_baud = if is_kline && init_settings == Some(1) {
                    // Spec-mandated 5-baud contract: no optional message is
                    // allowed, and NumReceiveCycles governs keybyte delivery
                    // (1 = deliver, 0 = run the init but suppress delivery).
                    if !request.cop_data.is_empty() {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(
                            "cop_data must be empty when the start communication request uses \
                             5-baud initialization (CP_InitializationSettings == 1)",
                        ));
                    }
                    let num_receive_cycles = ctrl.map(|c| c.num_receive_cycles).unwrap_or(0);
                    if num_receive_cycles != 0 && num_receive_cycles != 1 {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(format!(
                            "num_receive_cycles must be 0 (run the 5-baud init, suppress key \
                             byte delivery) or 1 (deliver the ECU key bytes) for a 5-baud \
                             initialization start communication request, got {num_receive_cycles}"
                        )));
                    }
                    let resolved = binding.resolved();
                    let functional =
                        resolved.unum32.get(&PARAM_REQUEST_ADDR_MODE).copied() == Some(2);
                    let (addr_param, addr_name, default_addr) = if functional {
                        (PARAM_5BAUD_ADDR_FUNC, "CP_5BaudAddressFunc", 0x33u32)
                    } else {
                        (PARAM_5BAUD_ADDR_PHYS, "CP_5BaudAddressPhys", 0x01u32)
                    };
                    let address_value = resolved
                        .unum32
                        .get(&addr_param)
                        .copied()
                        .unwrap_or(default_addr);
                    if address_value > 0xFF {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(format!(
                            "{addr_name} value {address_value} is out of range for a 5-baud \
                             init address byte (must fit in 0..=0xFF)"
                        )));
                    }
                    Some(FiveBaudInit {
                        address: address_value as u8,
                        deliver_keybytes: num_receive_cycles == 1,
                    })
                } else if is_kline
                    && !request.cop_data.is_empty()
                    && call_time_sequence == Some(super::events::InitSequence::FiveBaud)
                {
                    // Legacy heuristic (pre-ADR-076): the raw client byte is
                    // the address. For PROTOCOL_UART_ECHO_BYTE_PS this is the
                    // *only* reachable branch (CP_InitializationSettings is
                    // outside this protocol's ComParam allowlist --
                    // comparam_support.rs -- so the spec-mandated branch
                    // above can never fire for it), so it gets the same
                    // NumReceiveCycles validation/deliver_keybytes decision
                    // that branch already applies (the backlog, PR #58 Codex review finding). ISO9141/
                    // ISO14230's own use of this branch predates
                    // cop_ctrl_data/ADR-076's ctrl-based model and keeps its
                    // deliberately-unconditional keybyte delivery, verified
                    // unchanged.
                    if link.base_hw_protocol_id() == j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS {
                        let num_receive_cycles = ctrl.map(|c| c.num_receive_cycles).unwrap_or(0);
                        if num_receive_cycles != 0 && num_receive_cycles != 1 {
                            self.primitives.lock().await.remove(&cop_handle);
                            return Err(Status::invalid_argument(format!(
                                "num_receive_cycles must be 0 (run the 5-baud init, suppress \
                                 key byte delivery) or 1 (deliver the ECU key bytes) for a \
                                 5-baud initialization start communication request, got \
                                 {num_receive_cycles}"
                            )));
                        }
                        Some(FiveBaudInit {
                            address: request.cop_data[0],
                            deliver_keybytes: num_receive_cycles == 1,
                        })
                    } else {
                        Some(FiveBaudInit {
                            address: request.cop_data[0],
                            deliver_keybytes: true,
                        })
                    }
                } else {
                    None
                };

                // Fast-init dispatch (ADR-075, extended by ADR-077 for the
                // wakeup-only case): `cop_data` is payload-only for K-line
                // protocols (ISO9141/ISO14230, the only ones `is_kline`
                // selects an init sequence for); the
                // KWP header (when a request is sent) is built here, at call
                // time, the same way `init_tx_flags` above is -- from
                // `binding.resolved()` and the UniqueRespIdTable snapshot
                // already bound (`entries`). Built (and size-validated) only
                // when the bound snapshot actually selects fast-init:
                // `select_init_sequence` is a pure function of
                // `binding.resolved()` + `cop_data`, so this call-time gate
                // always agrees with `five_baud` above (call-time-exclusive,
                // ADR-076), and a five-baud or skip-init COP is never
                // rejected over a frame it would not send.
                //
                // An empty `cop_data` means "skip init" UNLESS
                // `CP_InitializationSettings` is explicitly `2` (Fast) on a
                // K-line link, in which case the D-PDU API spec's fast-init
                // service request is OPTIONAL: the wakeup pattern alone is
                // sent, via `PassThruIoctl(FAST_INIT)` with a NULL input, and
                // no frame is built or size-validated (ADR-077). This is
                // narrower than `select_init_sequence`'s `Fast` result for
                // empty `cop_data`, which the legacy heuristic (param unset)
                // also returns on ISO14230 -- that path must keep meaning
                // "skip init entirely" for backward compatibility, so the
                // check here is on the EXPLICIT bound param value, not on
                // `call_time_sequence`. Non-K-line links and every other
                // empty-`cop_data` case get `None` -- no fast-init at all.
                let fast_init = if !request.cop_data.is_empty()
                    && is_kline
                    && call_time_sequence == Some(super::events::InitSequence::Fast)
                {
                    match tx_header::build_tx_message(
                        // ADR-157 Plane B: normalized -- header format is a
                        // protocol-family decision.
                        ChannelProtocol::from_raw(resources::base_protocol_id(link.hw_protocol_id)),
                        tx_header::AddrModeSource::Request,
                        binding.resolved(),
                        &entries,
                        &request.cop_data,
                        link.software_isotp,
                        // This whole arm is gated on `is_kline` (ISO9141/
                        // ISO14230 only) -- TP2.0 never reaches here, so
                        // there is no established TX-ID to thread through.
                        None,
                        // ADR-198 Phase 2: K-line RawMode is now supported
                        // (extending ADR-196 Decision item 1's allowlist to
                        // ISO9141/ISO14230), so `link.raw_mode` can genuinely
                        // be `true` here -- `build_tx_message`'s own
                        // unconditional RawMode early-return (before its
                        // protocol match) then returns `request.cop_data`
                        // unchanged as `frame` below: the client's own
                        // complete fast-init request frame, header (and
                        // checksum iff ChecksumMode=OFF) included.
                        link.raw_mode,
                    ) {
                        Ok(frame) => {
                            // Same SAE J2534-1 size-range rejection the
                            // CoptSendrecv path applies to its constructed
                            // message (ADR-049): without it an oversized
                            // payload truncates the KWP length byte and only
                            // surfaces as an async init failure instead of a
                            // synchronous INVALID_ARGUMENT. K-line never uses
                            // extended addressing, so the Normal range applies.
                            // ADR-157 Plane B: normalized, same reasoning.
                            //
                            // ADR-198 Phase 2: widened by 1 byte (mirroring
                            // `resolve_send_recv_tx`'s identical
                            // `kline_manual_checksum_iso14230` adjustment)
                            // when this send's effective native
                            // `ISO9141_NO_CHECKSUM` connect flag is set
                            // (`raw_mode && !checksum_mode`) on an ISO14230
                            // link -- under RawMode=ON/ChecksumMode=OFF, the
                            // client's own fast-init frame includes its own
                            // manually-managed checksum byte.
                            let base_protocol = ChannelProtocol::from_raw(
                                resources::base_protocol_id(link.hw_protocol_id),
                            );
                            let base_size_range = base_protocol.tx_message_size_range(false);
                            let size_range = if link.raw_mode
                                && !link.checksum_mode
                                && base_protocol == ChannelProtocol::ISO14230
                            {
                                *base_size_range.start()..=(*base_size_range.end() + 1)
                            } else {
                                base_size_range
                            };
                            if !size_range.contains(&frame.len()) {
                                self.primitives.lock().await.remove(&cop_handle);
                                return Err(Status::invalid_argument(format!(
                                    "init_data length {} produces a {}-byte J2534 fast-init \
                                     message, outside the valid TX message size range \
                                     ({}..={} bytes) for this protocol",
                                    request.cop_data.len(),
                                    frame.len(),
                                    size_range.start(),
                                    size_range.end(),
                                )));
                            }
                            Some(FastInit::WithRequest(frame))
                        }
                        Err(err) => {
                            self.primitives.lock().await.remove(&cop_handle);
                            return Err(Status::invalid_argument(err));
                        }
                    }
                } else if request.cop_data.is_empty() && is_kline && init_settings == Some(2) {
                    Some(FastInit::WakeupOnly)
                } else {
                    None
                };

                // ISO 22900-2 §9.2.6.3.2 b): CAN/J1850 CoptStartcomm may carry an optional
                // request message; if NumReceiveCycles != 0, wait for a response. Resolved
                // eagerly here (ADR-111), against binding.resolved() -- Working when
                // temp_param_update was set, since (unlike CoptStopcomm) StartComm's Temp
                // binding is genuinely pushed to hardware for this transaction.
                let tx: Option<Box<OneShotCommTx>> = if is_kline || request.cop_data.is_empty() {
                    None
                } else {
                    // Fix 4 (Codex review, P2, PR #101, ADR-192/Phase 7
                    // Stage 7c): the same raw out-of-range
                    // `CP_TP20BroadcastAddress` rejection `CoptSendrecv`
                    // applies above, extended to this optional-message path
                    // -- see `validate_tp20_broadcast_address_range`'s own
                    // doc comment for why this must not be skipped here.
                    // ADR-210 Decision item 9: re-keyed from the narrow
                    // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id`
                    // (a `_CHx`-connected TP2.0 link's `CoptStartcomm`
                    // optional message would otherwise skip this rejection).
                    if resources::is_tp2_0_family_protocol_id(link.hw_protocol_id)
                        && let Err(err) = validate_tp20_broadcast_address_range(binding.resolved())
                    {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(err));
                    }
                    match resolve_send_recv_tx(
                        link.protocol,
                        link.hw_protocol_id,
                        binding.resolved(),
                        &entries,
                        &request.cop_data,
                        link.software_isotp,
                        base_tx_flags,
                        tp20_established_tx_id,
                        link.raw_mode,
                        link.checksum_mode,
                    ) {
                        Ok(resolved) => {
                            // ADR-196 Decision item 3b: same shared helper as
                            // the `CoptSendrecv` site above -- see
                            // `compute_tx_prefix`'s own doc comment.
                            let tx_prefix = compute_tx_prefix(
                                link.raw_mode,
                                link.hw_protocol_id,
                                resolved.tx_flags,
                                &request.cop_data,
                            );
                            let send = SendRecvTx {
                                data: resolved.data,
                                tx_flags: resolved.tx_flags,
                                isotp_tx: resolved.isotp_tx,
                                can_functional: resolved.can_functional,
                                request_sid: request.cop_data.get(tx_prefix).copied(),
                                access_timing_request: (!request.cop_data.is_empty())
                                    .then(|| request.cop_data.clone()),
                                j1939_tx_source: resolved.j1939_tx_source,
                                tp20_established_tx_id: resolved.tp20_established_tx_id,
                                tp20_is_broadcast: resolved.tp20_is_broadcast,
                                tx_prefix,
                            };
                            let expected_response: Vec<ExpectedResponse> = ctrl
                                .map(|c| {
                                    c.expected_response_array
                                        .iter()
                                        .map(|e| ExpectedResponse {
                                            mask: e.mask_data.clone(),
                                            pattern: e.pattern_data.clone(),
                                            unique_resp_ids: e.unique_resp_ids.clone(),
                                            acceptance_id: e.acceptance_id,
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let num_receive_cycles =
                                ctrl.map(|c| c.num_receive_cycles).unwrap_or(0);
                            if num_receive_cycles < -2 {
                                self.primitives.lock().await.remove(&cop_handle);
                                return Err(Status::invalid_argument(format!(
                                    "num_receive_cycles must be >= -2 (-1 = IS-CYCLIC, -2 = IS-MULTIPLE), got {num_receive_cycles}"
                                )));
                            }
                            if num_receive_cycles == -1 {
                                self.primitives.lock().await.remove(&cop_handle);
                                return Err(Status::invalid_argument(
                                    "num_receive_cycles must not be -1 (IS-CYCLIC) for the optional CoptStartcomm \
                                     message: the COP must terminate to reach PDU_CLLST_COMM_STARTED; use 0, n > 0, \
                                     or -2 (IS-MULTIPLE)",
                                ));
                            }
                            // ADR-157 Plane B: normalized -- RC21/23/78
                            // handling is a protocol-family decision.
                            let rc_cfg = RcHandlingConfig::from_params(
                                binding.resolved(),
                                ChannelProtocol::from_raw(resources::base_protocol_id(
                                    link.hw_protocol_id,
                                )),
                            )
                            .with_request_sid(send.request_sid);
                            let timing_cfg =
                                TimingChangeConfig::from_params(binding.resolved(), link.protocol)
                                    .map(|cfg| {
                                        Box::new(cfg.with_request(
                                            send.access_timing_request.as_deref().unwrap_or(&[]),
                                            send.tx_prefix,
                                        ))
                                    });
                            Some(Box::new(OneShotCommTx {
                                send,
                                expected_response,
                                num_receive_cycles,
                                response_timeout_ms: binding.resolved().p2_max_timeout_ms(),
                                rc_cfg,
                                timing_cfg,
                                enable_concatenation: binding.resolved().enable_concatenation(),
                            }))
                        }
                        Err(err) => {
                            self.primitives.lock().await.remove(&cop_handle);
                            return Err(Status::invalid_argument(err));
                        }
                    }
                };

                if tx_queue
                    .send(TxItem::StartComm {
                        cop_handle,
                        cll_handle: handle,
                        protocol_id: link.hw_protocol_id,
                        base_protocol_id: link.base_hw_protocol_id(),
                        tester_present,
                        init_tx_flags,
                        tx,
                        five_baud,
                        fast_init,
                        binding: Box::new(binding),
                        connect_generation,
                    })
                    .is_err()
                {
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::internal("tx queue closed unexpectedly"));
                }
                // PduCopstExecuting, PduCllstCommStarted, and PduCopstFinished are
                // emitted by the poll task.
            }

            vci_service_interface::ComOperationType::CoptStopcomm => {
                // Re-check comm_started under the lock to close the TOCTOU
                // window, and atomically test-and-set stop_comm_pending in
                // the SAME critical section so a second concurrent
                // CoptStopcomm is rejected synchronously rather than being
                // accepted while the first one's stop-comm sequence is still
                // executing (ADR-085 amendment).
                // `connect_generation` is NOT re-read here: it was already
                // captured, and consistency-checked against the RPC's
                // initial `link` snapshot, by the shared ADR-067 critical
                // section above (ADR-086, round 5) -- re-reading it in this
                // later block would shadow that trustworthy capture with a
                // THIRD, even-later read that could observe a reconnect
                // this call never actually raced against consistently.
                {
                    let mut links = self.logical_links.lock().await;
                    let link = links.get_mut(&handle);
                    if !link.as_ref().map(|l| l.comm_started).unwrap_or(true) {
                        let last_error = link.as_ref().and_then(|l| l.last_error.clone());
                        drop(links);
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(state_guard_status(
                            Code::FailedPrecondition,
                            "comm is not started; CoptStopcomm requires a prior successful CoptStartcomm",
                            PduError::PduErrCllNotStarted,
                            last_error,
                        ));
                    }
                    if link.as_ref().map(|l| l.stop_comm_pending).unwrap_or(false) {
                        let last_error = link.as_ref().and_then(|l| l.last_error.clone());
                        drop(links);
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(state_guard_status(
                            Code::FailedPrecondition,
                            "PDU_ERR_RESOURCE_BUSY: a CoptStopcomm is already in progress for \
                             this ComLogicalLink",
                            PduError::PduErrResourceBusy,
                            last_error,
                        ));
                    }
                    if link.as_ref().map(|l| l.connect_generation) != Some(connect_generation) {
                        let last_error = link.as_ref().and_then(|l| l.last_error.clone());
                        drop(links);
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(state_guard_status(
                            Code::FailedPrecondition,
                            "the ComLogicalLink was disconnected and reconnected since this call \
                             began; CoptStopcomm no longer targets the communication session it \
                             was issued against",
                            PduError::PduErrCllNotConnected,
                            last_error,
                        ));
                    }
                    if let Some(link) = link {
                        link.stop_comm_pending = true;
                    }
                }

                // ADR-085/ADR-087: non-empty cop_data on CoptStopcomm
                // transmits a final message, resolved eagerly here via the
                // same pipeline CoptSendrecv uses -- always against the bound
                // Active snapshot (never Working; temp_param_update remains a
                // no-op for CoptStopcomm, ADR-044/ADR-067 unchanged). Since
                // ADR-087, expected_response_array/NumReceiveCycles are also
                // parsed here and bundled into OneShotCommTx, so the poll task
                // can run a bounded, non-cancellable receive phase after the
                // transmit exactly like CoptSendrecv's own receive phase
                // (NumReceiveCycles == 0 stays fire-and-forget). Empty
                // cop_data preserves the pre-ADR-085 behavior exactly: no
                // transmit, no receive, no resolution.
                //
                // Resolved BEFORE the queued-primitives cancellation below: a
                // synchronous rejection here (oversized/malformed cop_data)
                // must leave every other queued ComPrimitive on this link
                // untouched, exactly as a rejected CoptStopcomm today leaves
                // the link's comm_started state untouched (Codex-review fix
                // -- an earlier version resolved this after cancellation,
                // so a rejected call still cancelled sibling COPs it never
                // actually superseded).
                let tx: Option<Box<OneShotCommTx>> = if request.cop_data.is_empty() {
                    None
                } else {
                    let ctrl = request.cop_ctrl_data.as_ref();
                    let tx_flags = ctrl
                        .map(|c| compute_j2534_tx_flags(c, link.raw_mode, link.hw_protocol_id))
                        .unwrap_or(0);
                    let (_, bound_active) = bound_comparams.clone().unwrap_or_default();
                    // Fix 4 (Codex review, P2, PR #101, ADR-192/Phase 7 Stage
                    // 7c): the same raw out-of-range
                    // `CP_TP20BroadcastAddress` rejection `CoptSendrecv`
                    // applies, extended to this optional-message path -- see
                    // `validate_tp20_broadcast_address_range`'s own doc
                    // comment for why this must not be skipped here.
                    // CoptStopcomm always resolves against Active (never
                    // Working -- `temp_param_update` is a no-op for
                    // CoptStopcomm, ADR-044/ADR-067), so `bound_active` is
                    // validated, matching `resolve_send_recv_tx`'s own
                    // `&bound_active` argument just below.
                    // ADR-210 Decision item 9: re-keyed from the narrow
                    // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id`
                    // (a `_CHx`-connected TP2.0 link's `CoptStopcomm`
                    // optional message would otherwise skip this rejection).
                    if resources::is_tp2_0_family_protocol_id(link.hw_protocol_id)
                        && let Err(err) = validate_tp20_broadcast_address_range(&bound_active)
                    {
                        self.rollback_stop_comm_pending(handle, cop_handle, connect_generation)
                            .await;
                        return Err(Status::invalid_argument(err));
                    }
                    match resolve_send_recv_tx(
                        link.protocol,
                        link.hw_protocol_id,
                        &bound_active,
                        &bound_active_table,
                        &request.cop_data,
                        link.software_isotp,
                        tx_flags,
                        tp20_established_tx_id,
                        link.raw_mode,
                        link.checksum_mode,
                    ) {
                        Ok(resolved) => {
                            // ADR-196 Decision item 3b: same shared helper as
                            // the `CoptSendrecv`/`CoptStartcomm` sites above
                            // -- see `compute_tx_prefix`'s own doc comment.
                            let tx_prefix = compute_tx_prefix(
                                link.raw_mode,
                                link.hw_protocol_id,
                                resolved.tx_flags,
                                &request.cop_data,
                            );
                            let send = SendRecvTx {
                                data: resolved.data,
                                tx_flags: resolved.tx_flags,
                                isotp_tx: resolved.isotp_tx,
                                can_functional: resolved.can_functional,
                                request_sid: request.cop_data.get(tx_prefix).copied(),
                                access_timing_request: (!request.cop_data.is_empty())
                                    .then(|| request.cop_data.clone()),
                                j1939_tx_source: resolved.j1939_tx_source,
                                tp20_established_tx_id: resolved.tp20_established_tx_id,
                                tp20_is_broadcast: resolved.tp20_is_broadcast,
                                tx_prefix,
                            };

                            // ADR-087: parse the same expected-response
                            // descriptors CoptSendrecv parses, so a client
                            // can define what "a response" to this final
                            // message means (e.g. KWP2000 StopCommunication's
                            // positive response).
                            let expected_response: Vec<ExpectedResponse> = ctrl
                                .map(|c| {
                                    c.expected_response_array
                                        .iter()
                                        .map(|e| ExpectedResponse {
                                            mask: e.mask_data.clone(),
                                            pattern: e.pattern_data.clone(),
                                            unique_resp_ids: e.unique_resp_ids.clone(),
                                            acceptance_id: e.acceptance_id,
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let num_receive_cycles =
                                ctrl.map(|c| c.num_receive_cycles).unwrap_or(0);

                            if num_receive_cycles < -2 {
                                self.rollback_stop_comm_pending(
                                    handle,
                                    cop_handle,
                                    connect_generation,
                                )
                                .await;
                                return Err(Status::invalid_argument(format!(
                                    "num_receive_cycles must be >= -2 (-1 = IS-CYCLIC, -2 = \
                                     IS-MULTIPLE), got {num_receive_cycles}"
                                )));
                            }
                            if num_receive_cycles == -1 {
                                self.rollback_stop_comm_pending(
                                    handle,
                                    cop_handle,
                                    connect_generation,
                                )
                                .await;
                                return Err(Status::invalid_argument(
                                    "num_receive_cycles must not be -1 (IS-CYCLIC) for \
                                     CoptStopcomm: the COP must terminate to return the \
                                     ComLogicalLink to PDU_CLLST_ONLINE; use 0, n > 0, or -2 \
                                     (IS-MULTIPLE)",
                                ));
                            }

                            // ADR-157 Plane B: normalized, same reasoning as
                            // the CoptStartcomm site above.
                            let rc_cfg = RcHandlingConfig::from_params(
                                &bound_active,
                                ChannelProtocol::from_raw(resources::base_protocol_id(
                                    link.hw_protocol_id,
                                )),
                            )
                            .with_request_sid(send.request_sid);
                            let timing_cfg =
                                TimingChangeConfig::from_params(&bound_active, link.protocol).map(
                                    |cfg| {
                                        Box::new(cfg.with_request(
                                            send.access_timing_request.as_deref().unwrap_or(&[]),
                                            send.tx_prefix,
                                        ))
                                    },
                                );
                            Some(Box::new(OneShotCommTx {
                                send,
                                expected_response,
                                num_receive_cycles,
                                response_timeout_ms: bound_active.p2_max_timeout_ms(),
                                rc_cfg,
                                timing_cfg,
                                enable_concatenation: bound_active.enable_concatenation(),
                            }))
                        }
                        Err(err) => {
                            self.rollback_stop_comm_pending(handle, cop_handle, connect_generation)
                                .await;
                            return Err(Status::invalid_argument(err));
                        }
                    }
                };

                // Cancel all queued primitives for this link so they do not
                // execute after StopComm is processed.  Primitives that are
                // already executing complete normally (best-effort); the poll
                // task emits PduCopstCancelled for each cancelled item when it
                // dequeues it, before reaching the StopComm item.
                //
                // Do NOT remove from primitives here: keeping the entry allows
                // GetStatus to return PduCopstCancelled until the poll task
                // dequeues and removes each item in order.  Removing early would
                // cause GetStatus to return PduCopstFinished for a brief window.
                // ADR-192/Phase 7 Stage 7c edge-case-hunter fix: a TP2.0
                // broadcast periodic COP (`link.tp20_broadcast_periodic`) has
                // its own out-of-band lifecycle -- a live native
                // `PassThruStartPeriodicMsg` message, stopped only by
                // `CoptCancel`, CLL teardown, or `CLEAR_PERIODIC_MSGS` -- and
                // must not be marked `cancelled_cops` by this sweep, or
                // `GetStatus` would report it `Cancelled` while its native
                // periodic message is still actually transmitting broadcast
                // frames on the wire.
                let broadcast_periodic_cop = self
                    .logical_links
                    .lock()
                    .await
                    .get(&handle)
                    .and_then(|l| l.tp20_broadcast_periodic.map(|p| p.cop_handle));
                let cops_to_cancel: Vec<u32> = {
                    let prims = self.primitives.lock().await;
                    prims
                        .iter()
                        .filter(|&(&c, entry)| {
                            entry.cll_handle == handle
                                && c != cop_handle
                                && Some(c) != broadcast_periodic_cop
                        })
                        .map(|(&c, _)| c)
                        .collect()
                };
                if !cops_to_cancel.is_empty() {
                    let mut links = self.logical_links.lock().await;
                    if let Some(link) = links.get_mut(&handle) {
                        // Codex review fix (P1, PR #101, round 10):
                        // `broadcast_periodic_cop` above was snapshotted from
                        // a strictly earlier `logical_links` lock
                        // acquisition than this extend. A concurrent
                        // `StartComPrimitive` publishing a fresh
                        // broadcast-periodic reservation into `link.
                        // tp20_broadcast_periodic` in that gap would be
                        // invisible to the stale snapshot and wrongly marked
                        // `cancelled_cops` here. Re-check fresh, under the
                        // same lock this block already holds.
                        let broadcast_periodic_cop_now: Option<u32> =
                            link.tp20_broadcast_periodic.map(|p| p.cop_handle);
                        link.cancelled_cops.extend(
                            cops_to_cancel
                                .iter()
                                .copied()
                                .filter(|c| Some(*c) != broadcast_periodic_cop_now),
                        );
                    }
                }
                // ADR-182 follow-up fix (Codex review round, PR #78): batch
                // sibling of `rpc_cancel_com_primitive`'s own self-check
                // (see that RPC's cancel-lookup comment above for the full
                // interleaving) -- a queued COP that the maintenance reap
                // (`events::reap_expired_cyclic_registrants`) fully finished,
                // including removing it from `primitives`, in the gap
                // between the read above and the `cancelled_cops.extend`
                // just above would otherwise leave its mark permanently
                // stale. Drain each one through the same shared helper.
                for &cop in &cops_to_cancel {
                    events::drain_cancelled_cop_if_finalized(
                        &self.primitives,
                        &self.logical_links,
                        handle,
                        cop,
                    )
                    .await;
                }

                if tx_queue
                    .send(TxItem::StopComm {
                        cop_handle,
                        cll_handle: handle,
                        protocol_id: link.hw_protocol_id,
                        tx,
                        connect_generation,
                    })
                    .is_err()
                {
                    self.rollback_stop_comm_pending(handle, cop_handle, connect_generation)
                        .await;
                    return Err(Status::internal("tx queue closed unexpectedly"));
                }
                // PduCopstExecuting, PduCllstOnline, and PduCopstFinished are
                // emitted by the poll task.
            }

            vci_service_interface::ComOperationType::CoptDelay => {
                let delay_ms = request.cop_ctrl_data.as_ref().map(|c| c.time).unwrap_or(0);
                if tx_queue
                    .send(TxItem::Delay {
                        cop_handle,
                        cll_handle: handle,
                        delay_ms,
                        connect_generation,
                    })
                    .is_err()
                {
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::internal("tx queue closed unexpectedly"));
                }
                // PduCopstExecuting and PduCopstFinished are emitted by the poll task
                // after the sleep completes.
            }

            vci_service_interface::ComOperationType::CoptUpdateparam => {
                // ADR-067 claim F / ADR-068: snapshot Working -- the
                // ComParam set AND the UniqueRespIdTable -- at call time, in
                // a SINGLE `logical_links` lock acquisition
                // (`update_param_working_snapshot`), so a `SetComParam`/
                // `SetUniqueRespIdTable` landing between two separate reads
                // cannot hand this COP a mixed-time (params, table) pair
                // that never actually existed together at any single
                // instant. A `SetComParam`/`SetUniqueRespIdTable` issued
                // after this call must not be promoted by it.
                // `handle_update_param` applies `params` to hardware and,
                // only on success, sets Active := params (in-memory Active
                // still updates at execution time on hw success, same
                // timing as before ADR-067; only the Working *read* moved to
                // call time) and promotes `unique_resp_id_table` to Active,
                // reconciling hardware FLOW_CONTROL_FILTERs.
                let (
                    params,
                    unique_resp_id_table,
                    hw_protocol_id,
                    channel_key,
                    comm_started,
                    active_node_address,
                    connect_generation,
                ) = self.update_param_working_snapshot(handle).await;

                // ADR-158 correction (Codex review, PR #30, round 3):
                // `CoptUpdateparam` has no path to substitute the physical
                // channel the way `ConnectComLogicalLink`'s `apply_fd_mode`
                // does -- ISO 22900-2's PDU_COPT_UPDATEPARAM is a working ->
                // active ComParam buffer transfer with no channel-lifecycle
                // role, and `CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate` have
                // no native `SET_CONFIG` mapping at all
                // (`ComParamId::to_j2534_config_id`'s `_ => false` catch-all)
                // -- so promoting a Working snapshot whose FD signal
                // contradicts the CLL's already-connected, connect-latched
                // channel identity would silently diverge Active from
                // hardware in either direction: a classic/ISO15765-connected
                // link would claim FD in `GetComParam` while the channel
                // stays classic, or an FD-connected link would claim classic
                // while the channel stays FD (and `ChannelKey`/lock
                // comparisons/`to_j2534_config_id`'s `BIT_SAMPLE_POINT`/
                // `SYNC_JUMP_WIDTH` suppression all still see FD). Reject
                // outright instead, mirroring `apply_fd_mode`'s own
                // established "reject rather than silently ignore" pattern
                // for every other out-of-scope FD combination. Checked at
                // call time (this snapshot), not inside the poll task's
                // `handle_update_param`: `hw_protocol_id` cannot change
                // between now and execution without a disconnect/reconnect,
                // and that path already bumps `connect_generation`, which
                // `handle_update_param`'s own U1 staleness check (ADR-086)
                // already cancels this COP for -- an execution-time re-check
                // would add concurrency surface for no additional coverage.
                if fd_mode_staged(&params) != resources::is_fd_protocol_id(hw_protocol_id) {
                    self.primitives.lock().await.remove(&cop_handle);
                    // ADR-159 correction: this branch predates Stage 3b and used to
                    // pick its message by `base_protocol_id(hw_protocol_id) != CAN`,
                    // which was correct back when CAN was the only FD-supporting
                    // family -- but once ISO15765 became one too (ADR-159), that
                    // condition wrongly matched a genuinely-supported, connect-time-
                    // latched ISO15765 mismatch and reported the stale "CAN FD is
                    // only supported on the CAN family... clause 22 is a future
                    // stage" message for the very feature this ADR ships. Now keyed
                    // on both FD-supporting families explicitly (`apply_fd_mode`'s
                    // own family dispatch), so the "out of scope" message is
                    // reserved for a family that genuinely isn't one of the two
                    // (unreachable in practice today -- `is_param_allowed`'s
                    // CAN-family gate already blocks staging either FD trigger
                    // ComParam anywhere else -- kept as a defensive fallback).
                    return Err(Status::invalid_argument(
                        if matches!(
                            resources::base_protocol_id(hw_protocol_id),
                            j2534_0404::CAN | j2534_0404::ISO15765
                        ) {
                            "this CoptUpdateparam's Working ComParams would change this \
                         ComLogicalLink's CAN FD mode, but FD-vs-Classic is a connect-time-latched \
                         physical channel property that CoptUpdateparam cannot change live -- \
                         disconnect and reconnect with the desired CP_CANFDTxMaxDataLength/\
                         CP_CANFDBaudrate staged, or CoptRestoreParam to discard the pending \
                         change"
                        } else {
                            "this CoptUpdateparam's Working ComParams request SAE J2534-2 CAN FD mode \
                         (CP_CANFDTxMaxDataLength/CP_CANFDBaudrate), but this ComLogicalLink's \
                         base hardware protocol is neither CAN nor ISO15765 -- CAN FD is only \
                         supported on those two families (SAE J2534-2 clauses 21/22)"
                        },
                    ));
                }

                // SAE J2534-2 clause 16 SAE J1939 (Codex review, PR #72
                // round 7): the StartComPrimitive-time `CP_J1939TargetAddress`
                // gate (`0xFFFF` sentinel / one-byte-range check, just above
                // this match in the `CoptStartcomm` arm) is not repeated
                // here -- a running J1939 CLL can stage a new
                // `CP_J1939TargetAddress` and promote it live via
                // `CoptUpdateparam`, whose Working snapshot the very next
                // `CoptSendrecv` reads through `tx_header::j1939_header_bytes`'s
                // `target_address as u8` cast with no validation of its own:
                // `0xFFFF` silently becomes `0xFF` (BAM broadcast, never
                // requested) and e.g. `0x100` silently becomes `0x00`, while
                // `GetComParam` keeps reporting the untruncated staged
                // value. Rejected here at call time, mirroring the FD-mode
                // guard's own "checked at call time, not inside
                // `handle_update_param`" reasoning just above: `hw_protocol_id`
                // cannot change between now and execution without a
                // disconnect/reconnect, which already bumps
                // `connect_generation` and is independently caught by
                // `handle_update_param`'s own U1 staleness check (ADR-086).
                //
                // The one-byte-range check (excluding the `0xFFFF` sentinel
                // itself, which is numerically > 0xFF but is not an
                // out-of-range value -- it is the documented "not
                // configured" marker) applies regardless of `comm_started`:
                // any OTHER value outside 0x00-0xFF is never valid,
                // truncates the same way whether staged before or after
                // `CoptStartcomm`, and is symmetric with the pre-flight
                // `CoptStartcomm` check. The `0xFFFF` sentinel check is
                // gated on `comm_started` specifically: `0xFFFF` is
                // `CP_J1939TargetAddress`'s own documented default
                // (`comparam_defaults.rs`) and stays the correct value for
                // every ComParam this CLL has not yet started communicating
                // with -- rejecting it unconditionally would block an
                // ordinary `CoptUpdateparam` staging an unrelated ComParam
                // (e.g. `CP_J1939PreferredAddress`) on a connected-but-not-
                // yet-started CLL, the same pattern
                // `comparam_tx.rs::iso15765_plain_sendrecv_respects_queued_
                // updateparam_ordering` already exercises for a different
                // protocol. Only a CLL that has actually started (and so
                // has a live `tx_header::j1939_header_bytes` read depending
                // on a resolved target address) needs this rejection.
                if resources::is_j1939_protocol_id(hw_protocol_id) {
                    let j1939_target_address =
                        params.unum32.get(&PARAM_J1939_TARGET_ADDRESS).copied();
                    if comm_started && j1939_target_address == Some(0xFFFF) {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(
                            "this CoptUpdateparam's Working ComParams would leave \
                             CP_J1939TargetAddress at the 0xFFFF \"not configured\" sentinel \
                             on an already-started SAE J1939 ComLogicalLink",
                        ));
                    }
                    if let Some(addr) = j1939_target_address
                        && addr > 0xFF
                        && addr != 0xFFFF
                    {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(format!(
                            "this CoptUpdateparam's Working CP_J1939TargetAddress \
                             ({addr:#06x}) must fit in one byte (0x00-0xFF, \
                             or the 0xFFFF \"not configured\" sentinel)"
                        )));
                    }

                    // Codex review finding (PR #72): the StartComPrimitive-
                    // time `CP_J1939Name` length gate (just above, in the
                    // `CoptStartcomm` arm) is not repeated by construction --
                    // a running J1939 CLL can stage a new `CP_J1939Name` and
                    // promote it live via this very `CoptUpdateparam`, which
                    // `run_j1939_reclaim_duties` (`events_j1939_claim.rs`)
                    // reads directly from Active on a later spontaneous-loss
                    // reclaim, bypassing the StartComPrimitive gate entirely
                    // (no new StartComPrimitive call happens for a
                    // spontaneous reclaim). Rejected here at call time,
                    // mirroring the TargetAddress check just above and the
                    // FD-mode guard's own "checked at call time, not inside
                    // `handle_update_param`" reasoning. Unconditional (not
                    // gated on `comm_started`), matching TargetAddress's own
                    // one-byte-range check: a wrong-length NAME is invalid
                    // whether staged before or after `CoptStartcomm`, since
                    // any later claim/reclaim attempt reads whatever is
                    // staged at ITS OWN call time, not just this CLL's first
                    // one. An absent value (`None`, ComParam not touched by
                    // this update) is not rejected, nor is an explicit
                    // empty Bytefield (`len == 0`) -- the same documented
                    // "not configured" default `comparam_defaults.rs` seeds
                    // at connect time (see the StartComPrimitive-time gate's
                    // own comment above), which a client may legitimately
                    // re-stage to reset NAME to unconfigured. Only a
                    // NON-EMPTY value of the wrong length is rejected.
                    let j1939_name_len = params.bytes.get(&PARAM_J1939_NAME).map(Vec::len);
                    if j1939_name_len.is_some_and(|len| len != 0 && len != 8) {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(format!(
                            "this CoptUpdateparam's Working CP_J1939Name ({} bytes) must be \
                             exactly 8 bytes (clause 16.3.3.2's NAME field)",
                            j1939_name_len.expect("is_some_and guarantees Some")
                        )));
                    }

                    // ADR-180 Decision 12 (design-advisor consult, Codex
                    // review PR #72 round 10) / Decision 23 (round-25
                    // correction): both `CP_TesterSourceAddress` (native
                    // `NODE_ADDRESS`) and `CP_J1939Name` are negotiation-owned
                    // once a claim (or claim attempt) is registered for this
                    // CLL -- ADR-179 Decision 3's claim loop writes the
                    // successfully claimed address into `NODE_ADDRESS` on
                    // both Working and Active, and registers the exact NAME
                    // it issued the native claim under in `SharedChannel::
                    // j1939_claims`. Promoting a DIFFERENT staged value for
                    // EITHER field here would move Active out from under the
                    // claim machinery -- `LogicalLinkState::j1939_claimed_
                    // address`, `SharedChannel::j1939_claims`, and the
                    // adapter's own native defense all stay unaware of the
                    // change, so every subsequent transmission (for
                    // `NODE_ADDRESS`) or claim/reclaim attempt (for NAME --
                    // `run_j1939_reclaim_duties` reads Active's NAME directly
                    // on a later spontaneous reclaim) diverges from what the
                    // adapter is actually defending.
                    //
                    // Gated on live `SharedChannel::j1939_claims` membership
                    // for this exact `(handle, connect_generation)` -- NOT on
                    // `CP_J1939AddressNegotiationRule`'s bit 1 (a client could
                    // flip negotiation off post-claim while the claim is
                    // still live and defended) and NOT on `comm_started`
                    // alone (an UpdateParam landing while a claim ATTEMPT is
                    // still in flight, before `comm_started` is set, would
                    // otherwise slip through and get clobbered once the
                    // attempt's own write-back runs) -- membership covers
                    // both. A CLL with no negotiation requested at all (bit 1
                    // set) never registers an entry here, so both fields stay
                    // fully client-owned for it, matching round 8's own
                    // "made client-writable" change. Hoisted into one shared
                    // `live_defended_name` read (round-25, retargeted round-26)
                    // instead of two separate `shared_channels` acquisitions
                    // computing the identical membership test -- a SEPARATE
                    // `self.shared_channels.lock().await` acquisition, AFTER
                    // `update_param_working_snapshot`'s own `logical_links`
                    // guard has already dropped -- sequential, not nested,
                    // matching that same guard's own lock-ordering note.
                    //
                    // ADR-180 Decision 23 round-26 correction: this now
                    // resolves to `Some(name)` from EITHER a live
                    // `j1939_claims` entry OR a pending reclaim
                    // (`j1939_reclaim_pending`) for this exact `(handle,
                    // connect_generation)`, carrying the NAME actually
                    // defended/being reclaimed rather than re-reading Active
                    // (which can be stale or never matched a Temp-bound
                    // claim in the first place). A pending reclaim counts
                    // too: without it, a restage landing during the brief
                    // loss-to-reclaim window would bypass this guard and get
                    // silently clobbered once the reclaim's own write-back
                    // runs -- the same "negotiation-owned while a claim or
                    // claim attempt is live" invariant this guard already
                    // claims to hold.
                    let live_defended_name: Option<[u8; 8]> = if let Some(key) = channel_key {
                        self.shared_channels.lock().await.get(&key).and_then(|sc| {
                            sc.j1939_claims
                                .values()
                                .find(|entry| {
                                    entry.cll_handle == handle
                                        && entry.connect_generation == connect_generation
                                })
                                .map(|entry| entry.name)
                                .or_else(|| {
                                    sc.j1939_reclaim_pending.get(&handle).and_then(|pending| {
                                        (pending.connect_generation == connect_generation)
                                            .then_some(pending.name)
                                    })
                                })
                        })
                    } else {
                        None
                    };

                    // Compared against Active (not `j1939_claimed_address`):
                    // an unrelated restage that leaves `CP_TesterSourceAddress`
                    // unchanged (e.g. re-staging the full ComParam set after a
                    // successful claim, where Working still equals the
                    // claimed Active value) must pass -- the same
                    // differs-from-applied shape the `CP_AnalogSampleRate`
                    // guard below already uses.
                    let staged_node_address = params
                        .unum32
                        .get(&ComParamId(j2534_0404::NODE_ADDRESS))
                        .copied()
                        .unwrap_or(0xF1);
                    if staged_node_address != active_node_address && live_defended_name.is_some() {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(
                            "this CoptUpdateparam's Working ComParams would move \
                             CP_TesterSourceAddress off the SAE J1939 address this \
                             ComLogicalLink's claim negotiation owns (ADR-179 Decision 3) -- \
                             the claimed source address is negotiation-owned while a claim or \
                             claim attempt is live; use CoptStopcomm, restage \
                             CP_J1939PreferredAddress, and re-run CoptStartcomm to claim a \
                             different address",
                        ));
                    }

                    // ADR-180 Decision 23 (design-advisor consult, Codex
                    // review PR #72; round-25 correction, round-26
                    // correction): the NAME counterpart of the
                    // `CP_TesterSourceAddress` guard just above -- see this
                    // whole block's own doc comment. `None` (`CP_J1939Name`
                    // never staged into Working at all) passes
                    // unconditionally, mirroring the length guard's own
                    // `None`-passes semantics above; only an actually-staged
                    // value (including an explicit empty restage, normalized
                    // to all-zero by [`events::normalize_j1939_name`]) is
                    // compared against `live_defended_name` -- the actually-
                    // issued NAME (from a live claim OR a pending reclaim),
                    // never Active's own `CP_J1939Name` (round-26: those can
                    // diverge for a Temp-bound claim never promoted to
                    // Active).
                    let staged_j1939_name = params.bytes.get(&PARAM_J1939_NAME);
                    if let Some(defended) = live_defended_name
                        && staged_j1939_name.is_some_and(|staged| {
                            events::normalize_j1939_name(Some(staged)) != defended
                        })
                    {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(
                            "this CoptUpdateparam's Working CP_J1939Name would move this \
                             ComLogicalLink's claim negotiation onto a different SAE J1939 NAME \
                             than the one it is currently claimed under (ADR-179 Decision 3) -- \
                             the claimed NAME is negotiation-owned while a claim or claim \
                             attempt is live; use CoptStopcomm, restage CP_J1939Name, and \
                             re-run CoptStartcomm to claim under a different NAME",
                        ));
                    }
                }

                // SAE J2534-2 clause 10 Analog Inputs (ADR-178): re-staging
                // CP_AnalogSampleRate away from the rate actually applied to
                // an already-connected analog channel is rejected outright,
                // mirroring the FD-mode guard just above -- silently
                // ignoring the re-stage would let GetComParam report a
                // Working value hardware was never reconfigured to match. A
                // channel_key miss (a disconnect raced between this
                // snapshot and this lookup) is not an error here:
                // handle_update_param's own connect_generation/ADR-086
                // staleness check catches that case at execution time, so
                // this guard skips silently rather than rejecting a
                // legitimate request on a benign race. This is a separate
                // `self.shared_channels.lock().await` acquisition, AFTER
                // `update_param_working_snapshot`'s own `logical_links`
                // guard has already dropped -- sequential, not nested, so no
                // new lock-ordering hazard under this codebase's
                // `shared_channels`-is-outermost-when-both-held-together
                // rule (they are never held together here).
                if resources::is_analog_in_protocol_id(hw_protocol_id)
                    && let Some(key) = channel_key
                    && let Some(sc) = self.shared_channels.lock().await.get(&key)
                {
                    let staged_rate = params
                        .unum32
                        .get(&PARAM_ANALOG_SAMPLE_RATE)
                        .copied()
                        .unwrap_or(0);
                    if Some(staged_rate) != sc.applied_analog_sample_rate {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(
                            "this CoptUpdateparam's Working ComParams would change this \
                             ComLogicalLink's CP_AnalogSampleRate, but the applied acquisition \
                             sample rate is a connect-time-latched physical channel property \
                             that CoptUpdateparam cannot change live (SAE J2534-2 clause 10) -- \
                             disconnect and reconnect with the desired CP_AnalogSampleRate \
                             staged, or CoptRestoreParam to discard the pending change",
                        ));
                    }
                }

                // SAE J2534-2 clause 10 Analog Inputs (ADR-216 Decision item
                // 10): extends the guard just above -- re-staging
                // CP_AnalogSamplesPerReading/CP_AnalogReadingsPerMsg away
                // from the value actually applied to an already-connected
                // analog channel is rejected outright, mirroring
                // CP_AnalogSampleRate's own value-comparison shape (NOT an
                // unconditional reject of every CoptUpdateparam on a
                // connected analog link -- CP_AnalogActiveChannels/
                // CP_AnalogAveragingMethod carry no such restriction and go
                // through this same CoptUpdateparam path unchanged). A
                // same-value re-stage is a legitimate no-op:
                // `apply_params_to_hardware_locked` (events.rs) never
                // re-forwards CONFIG_SAMPLES_PER_READING/
                // CONFIG_READINGS_PER_MSG to hardware post-connect at all
                // (they are connect-time-latched, exactly like
                // CONFIG_SAMPLE_RATE itself is forwarded once at connect via
                // `apply_j2534_params` and never again via this path), so
                // there is nothing to silently desync for an unchanged value
                // -- only a genuine attempted change needs pre-emptive
                // rejection, the same reasoning as the guard above.
                if resources::is_analog_in_protocol_id(hw_protocol_id)
                    && let Some(key) = channel_key
                    && let Some(sc) = self.shared_channels.lock().await.get(&key)
                {
                    let staged_samples_per_reading = params
                        .unum32
                        .get(&PARAM_ANALOG_SAMPLES_PER_READING)
                        .copied()
                        .unwrap_or(1);
                    let staged_readings_per_msg = params
                        .unum32
                        .get(&PARAM_ANALOG_READINGS_PER_MSG)
                        .copied()
                        .unwrap_or(1);
                    if Some(staged_samples_per_reading) != sc.applied_analog_samples_per_reading
                        || Some(staged_readings_per_msg) != sc.applied_analog_readings_per_msg
                    {
                        self.primitives.lock().await.remove(&cop_handle);
                        return Err(Status::invalid_argument(
                            "this CoptUpdateparam's Working ComParams would change this \
                             ComLogicalLink's CP_AnalogSamplesPerReading/CP_AnalogReadingsPerMsg, \
                             but the applied acquisition configuration is a connect-time-latched \
                             physical channel property that CoptUpdateparam cannot change live \
                             (SAE J2534-2 clause 10.3.3.2.3/.2.4) -- disconnect and reconnect with \
                             the desired values staged, or CoptRestoreParam to discard the pending \
                             change",
                        ));
                    }
                }

                if tx_queue
                    .send(TxItem::UpdateParam {
                        cop_handle,
                        cll_handle: handle,
                        params,
                        unique_resp_id_table,
                        connect_generation,
                    })
                    .is_err()
                {
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::internal("tx queue closed unexpectedly"));
                }
                // The poll task applies `params` to the hardware and, on
                // success, promotes it to Active, then emits
                // PduCopstExecuting + PduCopstFinished.
            }

            vci_service_interface::ComOperationType::CoptRestoreParam => {
                if tx_queue
                    .send(TxItem::RestoreParam {
                        cop_handle,
                        cll_handle: handle,
                        connect_generation,
                    })
                    .is_err()
                {
                    self.primitives.lock().await.remove(&cop_handle);
                    return Err(Status::internal("tx queue closed unexpectedly"));
                }
                // The poll task copies Active → Working, then emits
                // PduCopstExecuting + PduCopstFinished.
            }

            vci_service_interface::ComOperationType::CoptUnspecified => unreachable!(),
        }

        // ADR-067 claim D: temp_param_update writes Working back from Active
        // after a successful call, for all three temp-eligible COP types
        // (including CoptStopcomm, which touches no hardware but still
        // performs this writeback). Only reached once the COP has been
        // enqueued successfully -- a failure above (BUSTYPE guard, lock
        // conflict, resolution failure, closed tx queue) returns before this
        // point, so it never runs on a rejected call. The one EARLY `Ok`
        // return in this function (the broadcast-periodic branch's
        // `BroadcastPeriodicRevalidation::AlreadyResolved` arm) performs the
        // same writeback itself, via the same helper, so the invariant is
        // "every `Ok` return of an accepted COP," not "every path that
        // reaches this line" (edge-case-hunter finding, round 15 follow-up).
        self.apply_temp_param_working_writeback(handle, temp_param_update && temp_eligible)
            .await;

        debug!(
            cll_handle = handle,
            cop_handle,
            ?cop_type,
            "StartComPrimitive"
        );
        Ok(Response::new(vci_service_interface::ComPrimitiveResponse {
            cop_handle: Some(vci_service_interface::ComPrimitiveHandle {
                module_handle: DEFAULT_MODULE_HANDLE,
                cll_handle: handle,
                cop_handle,
            }),
        }))
    }

    /// ADR-067 claim D's `temp_param_update` Working-writeback, factored out
    /// of `rpc_start_com_primitive`'s `Ok` tail so its one early `Ok` return
    /// can perform the identical write rather than an open-coded copy
    /// (edge-case-hunter finding, round 15 follow-up, ADR-193).
    ///
    /// `enabled` is the caller's `temp_param_update && temp_eligible`. The
    /// criterion for running this is that the RPC accepted the COP and is
    /// returning `Ok` -- NOT that the staged Working snapshot was actually
    /// consumed by hardware. `CoptStopcomm` is the standing proof: it touches
    /// no hardware at all and still performs the writeback.
    async fn apply_temp_param_working_writeback(&self, cll_handle: u32, enabled: bool) {
        if !enabled {
            return;
        }
        let mut links = self.logical_links.lock().await;
        if let Some(l) = links.get_mut(&cll_handle) {
            l.working = l.active.clone();
        }
    }

    pub(super) async fn rpc_cancel_com_primitive(
        &self,
        request: Request<vci_service_interface::CancelComPrimitiveRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        let cop_handle = request
            .cop_handle
            .ok_or_else(|| Status::invalid_argument("cop_handle is required"))?
            .cop_handle;

        // Look up the associated cll_handle WITHOUT removing the COP from primitives.
        // Keeping it in primitives allows GetStatus to return PduCopstCancelled
        // (via the cancelled_cops check) instead of PduCopstFinished until the
        // poll task dequeues, skips, and removes the item.
        //
        // ADR-182 follow-up fix (Codex review round, PR #78): this lookup and
        // the mark-and-defer `cancelled_cops.insert` below happen under two
        // SEPARATE lock acquisitions, so the maintenance reap
        // (`events::reap_expired_cyclic_registrants`) can fully finish a
        // tier-2 registrant -- including removing it from `primitives` and
        // emitting its terminal status -- in the gap between them. That
        // interleaving is legally equivalent to the already-documented
        // ADR-128 already-terminal no-op-success outcome below (design-
        // advisor's traced interleaving, ADR-182's Consequences section),
        // never a silently-overridden cancel -- but it can leave the mark
        // this RPC is about to insert permanently stale, since nothing is
        // left alive to remove it. Closed by two cooperating cleanup sites,
        // both routed through `events::drain_cancelled_cop_if_finalized`:
        // this RPC's own self-check right after its mark-and-defer insert
        // below, and `reap_expired_cyclic_registrants`'s own late drain right
        // after its terminal emission (`events.rs`).
        let cll_handle = match self
            .primitives
            .lock()
            .await
            .get(&cop_handle)
            .map(|entry| entry.cll_handle)
        {
            Some(cll_handle) => cll_handle,
            None => {
                // A2-23 (ADR-128): a COP that already reached a terminal status
                // (Finished or Cancelled) but hasn't been destroyed (its CLL is
                // still alive) is a no-op success per §9.4.18.2 d) / §9.2.6.6 --
                // NOT PDU_ERR_INVALID_HANDLE. A handle absent from BOTH
                // `primitives` and `terminal_cops` never existed (or its CLL was
                // already destroyed), which is a genuine invalid handle.
                if self.terminal_cops.lock().await.lookup(cop_handle).is_some() {
                    debug!(
                        cop_handle,
                        "CancelComPrimitive: already terminal, no-op success"
                    );
                    return Ok(Self::empty_response());
                }
                return Err(unknown_handle_status(format!(
                    "unknown cop_handle {cop_handle}"
                )));
            }
        };

        // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c): a TP2.0
        // broadcast periodic COP was started directly via native
        // `PassThruStartPeriodicMsg`, bypassing the ordinary tx_queue/
        // poll-task dispatch pipeline entirely (Decision item 2) -- so it
        // never appears as a queued/held `TxItem` and does not participate
        // in that pipeline's own mark-and-defer cancellation machinery
        // below. Stop it directly here instead, mirroring the eager-
        // extraction shape the StopComm-only exception just below uses for
        // a different held-item class.
        {
            // Codex review fix (P2, PR #101, round 11 finding 2):
            // `connect_generation` and `channel_key` are captured in the SAME
            // critical section as `channel_id`/`periodic` -- mirroring the
            // "captured, not re-derived" principle
            // `terminate_tp20_broadcast_periodic_for_suspension`'s own
            // round-6/round-10 fixes established (`rpc_misc.rs`) -- so the
            // restore-vs-leak-track decision below can use the exact same
            // shared-session gate that helper uses, instead of the
            // unconditional restore this branch used before this fix.
            //
            // Codex review round 15 Fix 2 (P1, PR #101, ADR-193): that
            // critical section is now entered through the shared
            // `take_broadcast_periodic_under_api_locked` helper
            // (`rpc_misc.rs`), from INSIDE a `self.api` guard this branch
            // acquires first. `self.api` is the serialization fence for an
            // in-flight `PassThruStartPeriodicMsg`: taking a `None`-sentinel
            // reservation and reporting `PduCopstCancelled` under
            // `logical_links` alone -- what this branch used to do -- let the
            // client be told the COP was cancelled while the native start was
            // still in flight, after which SAE J2534-2 clause 19.3.2.3's
            // five-frame burst went out anyway. See that helper's own doc
            // comment for the full design and for what each of its three
            // outcomes means. The guard is held across the native
            // `stop_periodic_message` call below too (the committed-id case),
            // and dropped before the restore-or-leak-track/status work, which
            // needs no fence and must not hold `self.api` while acquiring
            // `shared_channels` (ADR-080).
            //
            // Deliberately unconditional, i.e. also for the overwhelmingly
            // common cancel of a COP that owns no broadcast periodic at all:
            // a "does this CLL currently track one for this cop_handle?"
            // pre-check would have to read `logical_links` WITHOUT the fence,
            // which is exactly the unsynchronized read this fix exists to
            // remove -- and it would buy only a briefly-held, uncontended
            // mutex on a path that is not hot (a cancel already contends with
            // the poll task for `self.api` whenever it does have a periodic
            // to stop).
            let api = self.api.lock().await;
            let taken = self
                .take_broadcast_periodic_under_api_locked(&api, cll_handle, cop_handle)
                .await;
            if let Some(TakenBroadcastPeriodic {
                periodic,
                channel_id,
                queue_target,
                connect_generation,
                channel_key,
            }) = taken
            {
                // Fix 2 (Codex review, P1, PR #101): a `None`-sentinel
                // `message_id` (`rpc_start_com_primitive`'s own reservation,
                // see `Tp20BroadcastPeriodic`'s doc comment) means this
                // COP's native `PassThruStartPeriodicMsg` call has not
                // committed a real id -- there is no real id to pass
                // `stop_periodic_message` at all. Skip the native call; the
                // tracking entry is already cleared above (the helper's own
                // `take()`), so this cancellation is still client-visible
                // immediately. `None` here can never be confused with a
                // genuine adapter-assigned id of `0` (Codex review, PR
                // #101): SAE J2534-1 clause 7.2.7.2 places no floor on
                // `pMsgID`, so `Some(PeriodicMessageId(0))` below IS treated
                // as a real, live message and IS stopped natively. As of
                // round 15 (ADR-193) this skip is no longer an accepted
                // residual: because the take happened under the `api` fence,
                // a start that had not yet revalidated will find its
                // reservation gone and never call the native start, and a
                // start that had already committed would have been observed
                // here as `Some(id)` instead of as the sentinel.
                // The native stop (when there is a real id to stop) runs
                // under the fence guard acquired above; `api` is then
                // released before ANY of the failure/finalization work
                // below, none of which needs it -- and
                // `restore_or_leak_track_broadcast_periodic` must not run
                // with `self.api` held, since it acquires `shared_channels`
                // (ADR-080's outermost lock). Hoisted out of the previous
                // `if let ... && let Err(err) = ...` chain for exactly that
                // reason, mirroring
                // `terminate_tp20_broadcast_periodic_for_suspension`'s own
                // `stop_result` shape (`rpc_misc.rs`).
                let stop_result = match (periodic.message_id, channel_id) {
                    (Some(message_id), Some(channel_id)) => Some((
                        message_id,
                        channel_id,
                        api.stop_periodic_message(channel_id, message_id),
                    )),
                    _ => None,
                };
                drop(api);
                if let Some((message_id, channel_id, Err(err))) = stop_result {
                    // Codex review fix (P2, PR #101, round 14): a
                    // channel-wide clear (e.g. `CLEAR_PERIODIC_MSGS`) that
                    // natively cleared this exact message's slot device-side
                    // AFTER `CoptCancel` already took this tracking entry off
                    // the link (`take()`, above) but BEFORE this native stop
                    // call ran surfaces here as `ERR_INVALID_MSG_ID` -- an
                    // AUTHORITATIVE "this id no longer exists device-side"
                    // signal, not a genuine failure. Treat it as
                    // already-cleaned-up and fall through to the ordinary
                    // success path below, completing the pattern already
                    // established three times elsewhere in this crate for
                    // the structurally identical situation:
                    // `retry_leaked_periodic_message_stops`'s own
                    // `ERR_INVALID_MSG_ID` branch (this exact
                    // `stop_periodic_message` call, on a leaked id),
                    // `retry_leaked_repeat_message_stops`'s `is_invalid_msg_id`
                    // branch (the `stop_repeat_message` sibling), and the
                    // `PDU_IOCTL_STOP_REPEAT_MESSAGE` handler's
                    // `result.is_ok() || is_invalid_msg_id` fallthrough (all
                    // three in `rpc_misc.rs`). A genuine (non-
                    // `ERR_INVALID_MSG_ID`) failure below keeps the pre-
                    // existing restore-or-leak-track-then-error behavior
                    // unchanged.
                    let is_invalid_msg_id = matches!(
                        &err,
                        j2534_0404::Error::ApiStatus { code, .. }
                            if code.as_u32() == j2534_0404::ERR_INVALID_MSG_ID
                    );
                    if is_invalid_msg_id {
                        debug!(
                            channel_id = channel_id.0,
                            message_id = message_id.0,
                            "CoptCancel TP2.0 broadcast periodic STOP got ERR_INVALID_MSG_ID -- \
                             device already forgot this message, treating as cleaned up"
                        );
                    } else {
                        // Fix 2 (Codex review round 2, P2, PR #101): unlike the
                        // `None`-sentinel skip above, a REAL native
                        // `PassThruStopPeriodicMsg` failure must not silently
                        // report success and lose all tracking -- the periodic
                        // message may still be actively transmitting broadcast
                        // frames device-side. Restore the SAME entry this branch
                        // just took off the link (`take()`, above) so a later
                        // `CoptCancel` retry on this cop_handle re-enters this
                        // same branch and attempts the native stop again --
                        // natural retry semantics, no new machinery needed. Do
                        // NOT remove the COP from `primitives` and do NOT report
                        // `PduCopstCancelled`: `rpc_get_status`'s own
                        // `is_broadcast_periodic` check (this file, above)
                        // continues correctly reporting `Executing` for as long
                        // as both stay intact, since the cancel genuinely didn't
                        // take effect. A bare gRPC error return (mirroring every
                        // other synchronous native-call failure in this file,
                        // e.g. the `PassThruStartPeriodicMsg` failure handling
                        // above) is sufficient -- no new COP-status machinery.
                        //
                        // Codex review fix (P2, PR #101, round 11 finding 2):
                        // whether this entry is actually safe to restore onto
                        // `link.tp20_broadcast_periodic` -- versus leak-tracked
                        // against the shared channel instead, because a
                        // concurrent disconnect (or disconnect+reconnect) raced
                        // this failure -- is now decided by the shared
                        // `restore_or_leak_track_broadcast_periodic` helper
                        // (`rpc_misc.rs`), the exact same gate
                        // `terminate_tp20_broadcast_periodic_for_suspension`
                        // uses. Before this fix, this branch restored
                        // unconditionally, with no session check at all -- the
                        // same hazard round 10 fixed for the suspension path but
                        // never applied here.
                        let last_error = self
                            .logical_links
                            .lock()
                            .await
                            .get(&cll_handle)
                            .and_then(|l| l.last_error.clone());
                        self.restore_or_leak_track_broadcast_periodic(
                            cll_handle,
                            periodic,
                            message_id,
                            CapturedBroadcastPeriodicSession {
                                connect_generation,
                                channel_key,
                                channel_id: Some(channel_id),
                            },
                            "CoptCancel",
                        )
                        .await;
                        return Err(map_native_error_for_link(
                            "PassThruStopPeriodicMsg",
                            &err,
                            last_error,
                        ));
                    }
                }
                let mut prims = self.primitives.lock().await;
                if let Some(entry) = prims.remove(&cop_handle) {
                    events::send_cop_status(
                        &self.subscriptions,
                        &self.terminal_cops,
                        queue_target.as_ref(),
                        cll_handle,
                        cop_handle,
                        vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled,
                        entry.cop_tag,
                    )
                    .await;
                }
                return Ok(Self::empty_response());
            }
        }

        // Mark the COP as cancelled so the poll task skips it when it dequeues
        // the corresponding TxItem.  The poll task sends PduCopstCancelled and
        // then removes the COP from primitives.
        // If the COP is already executing the poll task will let it complete
        // normally (best-effort cancellation for in-flight items).
        //
        // StopComm-only exception (ADR-085 round 5): a CoptStopcomm already
        // siphoned into `tx_held` (by PDU_IOCTL_SUSPEND_TX_QUEUE) is not
        // re-examined by anything until a later resume/clear/disconnect --
        // mark-and-defer alone would leave `stop_comm_pending` stuck `true`
        // indefinitely, permanently rejecting a retry with "already in
        // progress" even though GetStatus already reports this COP
        // Cancelled. So this RPC eagerly extracts a matching held
        // TxItem::StopComm and completes the cancellation synchronously,
        // right here. Every other held COP type keeps the existing
        // mark-and-defer behaviour: no guard depends on their timing, and
        // GetStatus already reports them Cancelled immediately via
        // `cancelled_cops` membership regardless of when the deferred event
        // fires, so there is nothing for them to gain from eager extraction.
        let (extracted_held_stop_comm, mark_inserted, queue_target) = {
            let mut links = self.logical_links.lock().await;
            match links.get_mut(&cll_handle) {
                Some(link) => {
                    if let Some(idx) = link.tx_held.iter().position(|item| {
                        matches!(item, TxItem::StopComm { cop_handle: c, .. } if *c == cop_handle)
                    }) {
                        link.tx_held.remove(idx);
                        link.stop_comm_pending = false; // same critical section, per ADR-085 convention
                        link.cancelled_cops.remove(&cop_handle); // defensive; normally absent
                        (true, false, Some(events::CllQueueTarget::from_link(link)))
                    } else {
                        link.cancelled_cops.insert(cop_handle); // unchanged mark-and-defer path
                        (false, true, Some(events::CllQueueTarget::from_link(link)))
                    }
                }
                None => (false, false, None),
            }
        };
        if extracted_held_stop_comm {
            // First-wins through `primitives`, the same discriminator every
            // other explicit-cancel CANCELLED emitter uses (ADR-118): hold
            // the lock across both the removal and the notification, so a
            // concurrent GetStatus never observes the entry gone before the
            // terminal CANCELLED has actually been sent. Mirrors
            // should_skip_cancelled_item's explicit-cancel branch.
            let mut prims = self.primitives.lock().await;
            if let Some(entry) = prims.remove(&cop_handle) {
                events::send_cop_status(
                    &self.subscriptions,
                    &self.terminal_cops,
                    queue_target.as_ref(),
                    cll_handle,
                    cop_handle,
                    vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled,
                    entry.cop_tag,
                )
                .await;
            }
        } else if mark_inserted {
            // ADR-182 follow-up fix (Codex review round, PR #78): self-check
            // for the interleaving where the maintenance reap
            // (`events::reap_expired_cyclic_registrants`) fully finished
            // this COP -- including removing it from `primitives` -- between
            // this RPC's own lookup above and the mark-and-defer insert just
            // above. That interleaving already resolves to the ADR-128
            // already-terminal no-op-success outcome (this RPC's own return
            // value below is unchanged, success, either way); this cleans up
            // the otherwise-permanently-stale `cancelled_cops` mark just
            // inserted, since nothing else is left alive to remove it. See
            // this function's own cancel-lookup comment above for the full
            // interleaving and `drain_cancelled_cop_if_finalized`'s doc
            // comment for the sibling cleanup site.
            if events::drain_cancelled_cop_if_finalized(
                &self.primitives,
                &self.logical_links,
                cll_handle,
                cop_handle,
            )
            .await
            {
                debug!(
                    cop_handle,
                    cll_handle,
                    "CancelComPrimitive: reap already finalized this COP before the mark could \
                     land, no-op success -- drained the stale cancelled_cops mark"
                );
            }
        }

        debug!(cop_handle, cll_handle, "CancelComPrimitive");
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_get_status(
        &self,
        request: Request<vci_service_interface::GetStatusRequest>,
    ) -> Result<Response<vci_service_interface::StatusResponse>, Status> {
        let request = request.into_inner();
        let timestamp = events::module_timestamp_us();

        let status = match request.handle {
            Some(vci_service_interface::get_status_request::Handle::ModuleHandle(handle)) => {
                Self::require_module_handle(Some(handle), self.modules.len())?;
                // ADR-132 Amendment 3 (Codex review, PR #145): mirror
                // `rpc_get_module_ids`'s own per-handle state machine
                // instead of returning the shared `module_state.status`
                // unconditionally for whatever handle was requested. Before
                // this fix, an unopened handle (including at startup, or
                // any handle other than whichever one is actually open)
                // reported `module_state`'s default/leftover value --
                // typically `PduModstReady` -- while `GetModuleIds` (since
                // this ADR's Decision) correctly reports `PduModstAvail`
                // for that same unopened handle: two RPCs disagreeing about
                // the same module's status for the same reason A2-25
                // flagged `GetModuleIds` itself for. Holds the `device_id`
                // guard across the `module_state` read for the same reason
                // `rpc_get_module_ids` does (Amendment 1): a
                // `ModuleDisconnect`+`ModuleConnect(other handle)` racing a
                // released-then-reacquired pair of locks could otherwise
                // attribute the freshly-opened module's status to this
                // handle.
                let device_id_guard = self.device_id.lock().await;
                let is_open_handle =
                    device_id_guard.as_ref().map(|(h, _)| *h) == Some(handle.module_handle);
                let status = if is_open_handle {
                    self.module_state.lock().await.status
                } else {
                    vci_service_interface::PduModuleStatus::PduModstAvail
                };
                drop(device_id_guard);
                vci_service_interface::status_response::Status::ModuleStatus(status as i32)
            }
            Some(vci_service_interface::get_status_request::Handle::CllHandle(h)) => {
                let cll_status = match self.get_link_state(h.cll_handle).await {
                    Ok(link) if link.connected && link.comm_started => {
                        vci_service_interface::PduComLogicalLinkStatus::PduCllstCommStarted
                    }
                    Ok(link) if link.connected => {
                        vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline
                    }
                    _ => vci_service_interface::PduComLogicalLinkStatus::PduCllstOffline,
                };
                vci_service_interface::status_response::Status::CllStatus(cll_status as i32)
            }
            Some(vci_service_interface::get_status_request::Handle::CopHandle(h)) => {
                let cop_handle = h.cop_handle;
                // Determine COP status by consulting: primitives, cancelled_cops, executing_cop.
                // `dispatched` is captured in the same lookup as `cll_h` (both
                // live inside the single `CopEntry` this cop_handle maps to),
                // so there is no separate lock acquisition -- and therefore no
                // race window -- between reading "which CLL" and "has this COP
                // ever been dispatched" (ADR-117).
                let maybe_entry = self
                    .primitives
                    .lock()
                    .await
                    .get(&cop_handle)
                    .map(|entry| (entry.cll_handle, entry.dispatched));
                let cop_status = if let Some((cll_h, dispatched)) = maybe_entry {
                    // COP is still in the queue or actively executing.
                    // Priority: Cancelled > Executing > Waiting.
                    let is_cancelled = {
                        self.logical_links
                            .lock()
                            .await
                            .get(&cll_h)
                            .map(|l| l.cancelled_cops.contains(&cop_handle))
                            .unwrap_or(false)
                    };
                    if is_cancelled {
                        vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled
                    } else {
                        // Check whether the poll task is currently executing this COP.
                        let channel_key = {
                            self.logical_links
                                .lock()
                                .await
                                .get(&cll_h)
                                .and_then(|l| l.channel_key)
                        };
                        let is_executing = if let Some(ck) = channel_key {
                            let chans = self.shared_channels.lock().await;
                            if let Some(sc) = chans.get(&ck) {
                                *sc.executing_cop.lock().await == Some(cop_handle)
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        // ADR-100 Decision §2, "Resolved": `executing_cop`
                        // stays a single slot meaning "the TxItem the poll
                        // task is dispatching right now" -- a detached
                        // (migrated) IS-CYCLIC registrant is definitionally
                        // not that once its first positive response has
                        // freed the poll task (S5), even though it is very
                        // much still executing per spec ("placed into a
                        // receive only mode"). So Executing is also reported
                        // when this cop_handle has a live tier-2
                        // (`RegistrantTier::ReceiveOnly`) registrant on this
                        // CLL -- Cancelled > (executing_cop match OR a live
                        // detached registrant) > Waiting.
                        let is_detached_tier2 = self
                            .logical_links
                            .lock()
                            .await
                            .get(&cll_h)
                            .is_some_and(|l| {
                                l.registrants.iter().any(|r| {
                                    r.cop_handle == cop_handle
                                        && r.tier == RegistrantTier::ReceiveOnly
                                })
                            });
                        // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage
                        // 7c): a live broadcast-periodic COP never touches
                        // `executing_cop` at all -- it was started directly
                        // by `rpc_start_com_primitive`, bypassing the
                        // ordinary tx_queue/poll-task dispatch pipeline
                        // entirely (Decision item 2), so `is_executing`
                        // above is always `false` for it, the same way a
                        // detached tier-2 registrant's own live state lives
                        // outside `executing_cop` too. Reported `Executing`
                        // for as long as `LogicalLinkState::
                        // tp20_broadcast_periodic` still names this COP --
                        // cleared by `CoptCancel`/CLL teardown/
                        // `CLEAR_PERIODIC_MSGS`'s own reconciliation, each of
                        // which also removes this COP from `primitives`
                        // (never leaving it stuck reporting `Executing`
                        // after the native periodic message is actually
                        // gone).
                        let is_broadcast_periodic = self
                            .logical_links
                            .lock()
                            .await
                            .get(&cll_h)
                            .and_then(|l| l.tp20_broadcast_periodic)
                            .is_some_and(|p| p.cop_handle == cop_handle);
                        if is_executing || is_detached_tier2 || is_broadcast_periodic {
                            vci_service_interface::PduComPrimitiveStatus::PduCopstExecuting
                        } else {
                            // Distinguish a never-dispatched COP (`Idle`) from
                            // a cyclic COP resting between send cycles
                            // (`Waiting`) via `CopEntry::dispatched`, captured
                            // above alongside `cll_h`. See ADR-117.
                            if dispatched {
                                vci_service_interface::PduComPrimitiveStatus::PduCopstWaiting
                            } else {
                                vci_service_interface::PduComPrimitiveStatus::PduCopstIdle
                            }
                        }
                    }
                } else {
                    // A2-23 (ADR-128): a COP that left `primitives` at terminal-emission
                    // time may still be in `terminal_cops` (its CLL hasn't been
                    // destroyed yet) -- report its real recorded terminal status rather
                    // than assuming Finished. Falls back to the pre-existing unconditional
                    // Finished default when `terminal_cops` also has no entry (a
                    // genuinely unknown handle -- separate, pre-existing gap, not
                    // addressed here).
                    self.terminal_cops
                        .lock()
                        .await
                        .lookup(cop_handle)
                        .map(|(_, status)| status)
                        .unwrap_or(vci_service_interface::PduComPrimitiveStatus::PduCopstFinished)
                };
                vci_service_interface::status_response::Status::CopStatus(cop_status as i32)
            }
            None => {
                return Err(Status::invalid_argument("handle is required"));
            }
        };

        Ok(Response::new(vci_service_interface::StatusResponse {
            timestamp,
            extra_info: 0,
            status: Some(status),
        }))
    }

    pub(super) async fn rpc_get_event_item(
        &self,
        request: Request<vci_service_interface::GetEventItemRequest>,
    ) -> Result<Response<vci_service_interface::EventItemResponse>, Status> {
        use std::sync::Arc;

        let request = request.into_inner();

        let event_item = match request.handle {
            Some(vci_service_interface::get_event_item_request::Handle::CllHandle(h)) => {
                // Read from the in-process rx_buf instead of calling PassThruReadMsgs
                // directly, so that this path does not race with the poll task which is
                // the sole caller of read_messages for this channel.
                let rx_buf = {
                    let links = self.logical_links.lock().await;
                    let link = links.get(&h.cll_handle).ok_or_else(|| {
                        unknown_handle_status(format!("unknown cll_handle {}", h.cll_handle))
                    })?;
                    // Allow draining rx_buf even after disconnect: frames buffered
                    // before DisconnectComLogicalLink must not be silently lost.
                    Arc::clone(&link.rx_buf)
                };

                // `result_buffer_limit` is read fresh, under this same
                // `rx_buf` lock, at the point of use (Codex review on PR #3,
                // ADR-140 follow-up -- see `CllEventQueue`'s own doc
                // comment) rather than snapshotted separately under
                // `logical_links` above.
                let mut queue = rx_buf.lock().await;
                let result_buffer_limit = queue.result_buffer_limit;
                // ADR-115 single-consumer correction: `cll_queue_item_to_event_item`
                // (`events.rs`) is the same conversion the live
                // `SubscribeEvent` drain path (`deliver_or_enqueue`) uses to
                // turn a backlog item into a notification -- factored out so
                // both paths apply the identical result_buffer_limit
                // truncation / ADR-051 header-footer split.
                //
                // ADR-146 deferred-finalization delivery (Codex round-5
                // finding, PR #17): peek before popping -- an unfinalized
                // `CllQueueItem::PendingTimingFrame` at the front must NOT be
                // popped (which would destroy/skip it once
                // `finalize_pending_timing_frame` later tries to find it by
                // `reservation_id`); this poller reports "nothing available
                // yet" instead and leaves it in place for a later poll (or a
                // live subscriber, once finalized).
                match queue.items.front() {
                    Some(CllQueueItem::PendingTimingFrame { .. }) => None,
                    _ => queue.items.pop_front().map(|item| {
                        super::events::cll_queue_item_to_event_item(
                            h.cll_handle,
                            result_buffer_limit,
                            &item,
                        )
                    }),
                }
            }
            Some(vci_service_interface::get_event_item_request::Handle::ModuleHandle(_)) => {
                self.module_event_buf.lock().await.pop_front()
            }
            Some(vci_service_interface::get_event_item_request::Handle::SystemHandle(_)) => {
                self.system_event_buf.lock().await.pop_front()
            }
            None => return Err(Status::invalid_argument("handle is required")),
        };

        Ok(Response::new(vci_service_interface::EventItemResponse {
            event_item,
        }))
    }

    /// ADR-115 round 6 (superseding round 4/5's generation-gate): writes
    /// `queue.live_sender = Some(tx.clone())` iff `subscription_key`'s
    /// CURRENT `subscriptions` entry still `same_channel`s `tx` -- the "am I
    /// still current" identity check `rpc_subscribe_event`'s reconciliation
    /// step needs before writing a queue out-of-band (i.e. from a re-check
    /// made after this call's own primary `subscriptions` insert has already
    /// completed). A clone of the same `mpsc::UnboundedSender` shares
    /// channel identity with the original (`same_channel`); a distinct
    /// `SubscribeEvent` call always produces a distinct channel, so this is
    /// a sufficient and unambiguous identity test: if a later
    /// `SubscribeEvent` call already displaced this one, `subs.get(key)`'s
    /// stored sender will be a different channel and this is a no-op -- a
    /// delayed/stale reconciliation must never stomp a newer,
    /// correctly-installed subscriber. Locks `subscriptions` then, if the
    /// check passes, the queue -- consistent with this crate's stated lock
    /// order (`logical_links` -> `subscriptions` -> per-CLL queue lock, see
    /// `J2534Service::logical_links`'s own doc comment).
    ///
    /// Factored out of `rpc_subscribe_event`'s reconciliation step
    /// specifically so this no-op behavior can be exercised directly by a
    /// unit test (`rpc_subscribe_event_live_sender_tests`, below) without
    /// needing to force a specific concurrent interleaving through
    /// `rpc_subscribe_event` itself -- reliably forcing that exact
    /// interleaving through this crate's `current_thread` test runtime is
    /// not practical (same class of narrow-window infeasibility as
    /// `rollback_stop_comm_pending_tests`, see that module's own doc
    /// comment).
    async fn reconcile_stale_cll_subscription(
        &self,
        subscription_key: SubscriptionKey,
        tx: &SubscriptionSender,
        queue: &Arc<Mutex<CllEventQueue>>,
    ) {
        let subs = self.subscriptions.lock().await;
        if subs
            .get(&subscription_key)
            .is_some_and(|current| current.same_channel(tx))
        {
            queue.lock().await.live_sender = Some(tx.clone());
        }
    }

    pub(super) async fn rpc_subscribe_event(
        &self,
        request: Request<vci_service_interface::SubscribeEventRequest>,
    ) -> Result<Response<EventStream>, Status> {
        let request = request.into_inner();

        // Use a (module_handle, cll_handle) tuple as the key so that module-level
        // subscriptions (module_handle = 1, cll_handle = PDU_HANDLE_UNDEF) do not
        // collide with CLL subscriptions whose cll_handle also starts from 1.
        let cll_handle = match &request.handle {
            Some(vci_service_interface::subscribe_event_request::Handle::CllHandle(h)) => {
                Some(h.cll_handle)
            }
            _ => None,
        };
        let subscription_key: SubscriptionKey = match &request.handle {
            Some(vci_service_interface::subscribe_event_request::Handle::CllHandle(h)) => {
                (DEFAULT_MODULE_HANDLE, h.cll_handle)
            }
            Some(vci_service_interface::subscribe_event_request::Handle::ModuleHandle(h)) => {
                (h.module_handle, PDU_HANDLE_UNDEF)
            }
            Some(vci_service_interface::subscribe_event_request::Handle::SystemHandle(_)) => {
                SYSTEM_SUBSCRIPTION_KEY
            }
            None => {
                return Err(Status::invalid_argument(
                    "handle is required for SubscribeEvent",
                ));
            }
        };

        let (tx, rx) = mpsc::unbounded_channel::<Result<EventNotification, Status>>();
        let tx_for_task = tx.clone();

        // ADR-115 round 6: pre-resolve the queue Arc BEFORE touching
        // `subscriptions` -- lock `logical_links`, clone the CLL's queue
        // (if the link exists), then drop that guard immediately. This is
        // only a read of what queue (if any) exists right now; the
        // decision of whether it is still the right queue to make live is
        // made atomically, under `subscriptions`, below (lock order:
        // `logical_links` -> `subscriptions` -> per-CLL queue lock, see
        // `J2534Service`'s own doc comment for this crate's stated lock
        // hierarchy).
        let queue_before = if let Some(cll_handle) = cll_handle {
            let links = self.logical_links.lock().await;
            links.get(&cll_handle).map(|l| Arc::clone(&l.rx_buf))
        } else {
            None
        };

        // ADR-115 round 6 (the atomicity fix, same shape as round 4/5 --
        // only what gets written changed): insert this call's `tx` into
        // `subscriptions` AND -- if `queue_before` found a queue -- write
        // that queue's `live_sender = Some(tx.clone())`, both in ONE
        // critical section held under the single `subscriptions` lock
        // acquisition (order: `subscriptions` -> queue, consistent with
        // the lock hierarchy above). Every concurrent `SubscribeEvent`
        // call for the same or a different key serializes through this
        // one `subscriptions` lock acquisition, so "decide who is in the
        // map" and "make their queue live" can never observably interleave
        // with another call's own insert+stamp -- and, since the queue
        // itself (not a value captured elsewhere) is what
        // `deliver_or_enqueue` consults, there is no separate stale-copy
        // class left to guard against on the read side either.
        let displaced = {
            let mut subs = self.subscriptions.lock().await;
            let displaced = subs.insert(subscription_key, tx.clone());
            if let Some(queue) = &queue_before {
                queue.lock().await.live_sender = Some(tx.clone());
            }
            displaced
        };
        if let Some(old) = displaced {
            let _ = old.send(Err(Status::cancelled(
                "Subscription replaced by a new subscriber for the same handle",
            )));
        }

        // ADR-115 round 6: reconciliation, run UNCONDITIONALLY -- not only
        // when `queue_before` found no link (a rejected earlier draft's
        // bug, caught in internal review before it ever reached Codex). A
        // destroy+recreate race landing between the `queue_before` read
        // above and the critical section above can make `queue_before`
        // stale even when it WAS `Some` (it would then be the old,
        // discarded queue's Arc, not the new link's). Re-resolve the queue
        // Arc the same way `queue_before` did; if the link changed
        // underneath (no queue before but one now, or a different queue
        // Arc now), reconcile -- the mirror image of
        // `rpc_create_com_logical_link`'s own reconciliation (`rpc_link.rs`),
        // which reads `subscriptions` for exactly this case.
        if let Some(cll_handle) = cll_handle {
            let queue_after = {
                let links = self.logical_links.lock().await;
                links.get(&cll_handle).map(|l| Arc::clone(&l.rx_buf))
            };
            let link_changed = match (&queue_before, &queue_after) {
                (_, None) => false,
                (None, Some(_)) => true,
                (Some(before), Some(after)) => !Arc::ptr_eq(before, after),
            };
            if link_changed {
                let queue = queue_after.expect("Some checked by link_changed above");
                self.reconcile_stale_cll_subscription(subscription_key, &tx, &queue)
                    .await;
            }
        }

        // Task: remove this subscription entry when the client disconnects or shutdown fires.
        let subs = std::sync::Arc::clone(&self.subscriptions);
        let mut shutdown_rx = self.shutdown.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown_rx.changed() => {}
                _ = tx_for_task.closed() => {}
            }
            let mut subs = subs.lock().await;
            if let Some(current) = subs.get(&subscription_key)
                && current.same_channel(&tx_for_task)
            {
                subs.remove(&subscription_key);
            }
        });

        Ok(Response::new(Box::pin(UnboundedReceiverStream::new(rx))))
    }
}

#[cfg(test)]
mod rollback_stop_comm_pending_tests {
    //! Direct unit-level coverage of `rollback_stop_comm_pending`'s
    //! generation gate (Codex review, PR #92): a genuine
    //! disconnect+reconnect-during-suspended-rollback race is infeasible to
    //! construct through the gRPC layer with this crate's established test
    //! techniques -- every one of the four rollback call sites reaches
    //! `rollback_stop_comm_pending` with no `.await` point (or only
    //! uncontended, therefore non-yielding, `.lock().await`s) between the
    //! generation check that accepted the call and the rollback itself, so
    //! there is no natural preemption opportunity for a concurrent
    //! reconnect to land in the window within this crate's single-threaded
    //! `current_thread` test runtime (see
    //! `j2534-0404-service/docs/implementation-notes.md`'s established
    //! infeasibility notes for this exact class of narrow suspension-window
    //! race). Instead, this test calls the extracted helper directly with a
    //! deliberately mismatched generation, exercising the same guard logic
    //! that would fire if the race above ever actually happened.
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;

    pub(super) const TEST_HANDLE: u32 = 1;
    const TEST_COP_HANDLE: u32 = 100;

    /// Builds a minimal `J2534Service` backed by the mock J2534 cdylib
    /// (already a `[dev-dependencies]` of this crate for `tests/grpc_mock`),
    /// with a single `LogicalLinkState` at `TEST_HANDLE` and one
    /// `self.primitives` entry at `TEST_COP_HANDLE`. Never drives an actual
    /// RPC or the poll task; only used to call private helpers directly.
    ///
    /// `pub(super)`: also reused by `rpc_subscribe_event_live_sender_tests`
    /// (below) for its own concurrent-`SubscribeEvent` and reconciliation
    /// tests -- both need the exact same minimal single-CLL `J2534Service`
    /// shape this already builds.
    pub(super) async fn service_with_one_link(
        connect_generation: u64,
        stop_comm_pending: bool,
    ) -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");

        let mut logical_links = HashMap::new();
        logical_links.insert(
            TEST_HANDLE,
            LogicalLinkState {
                channel_id: None,
                protocol: ChannelProtocol::CAN,
                hw_protocol_id: 0,
                software_isotp: false,
                uudt_channel_id: None,
                uudt_channel_key: None,
                isotp_rx: Arc::new(Mutex::new(HashMap::new())),
                connect_in_flight: std::sync::Weak::new(),
                connected: false,
                comm_started: true,
                raw_mode: false,
                checksum_mode: false,
                connect_generation,
                stop_comm_pending,
                channel_key: None,
                pin_select: None,
                channel_index: None,
                base_hw_protocol_override: None,
                rx_buf: Arc::new(Mutex::new(CllEventQueue {
                    event_queue_cap: 16,
                    ..CllEventQueue::default()
                })),
                working: ComParamSet::default(),
                active: ComParamSet::default(),
                tester_present_state: TesterPresentState::None,
                tester_present_base_tx_flags: 0,
                open_tp_discards: Vec::new(),
                working_unique_resp_id_table: Vec::new(),
                active_unique_resp_id_table: Vec::new(),
                unique_resp_filter_ids: Vec::new(),
                cancelled_cops: HashSet::new(),
                held_lock_mask: 0,
                last_error: None,
                tx_held: VecDeque::new(),
                tx_suspended_by_ioctl: false,
                tx_suspended_by_lock: false,
                tx_suspended_by_error: false,
                error_clear_seq: 0,
                error_set_seq: 0,
                client_filters: HashMap::new(),
                repeat_message_ids: Vec::new(),
                pending_client_filters: HashMap::new(),
                registrants: Vec::new(),
                next_registrant_seq: 0,
                j1939_claimed_address: None,
                j1939_claim_cursor: 0,
                j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
                tp20_connection: None,
                tp20_broadcast_periodic: None,
            },
        );

        let mut primitives = HashMap::new();
        primitives.insert(
            TEST_COP_HANDLE,
            CopEntry {
                cll_handle: TEST_HANDLE,
                dispatched: false,
                transmits: false,
                is_send_recv: false,
                cop_tag: None,
            },
        );

        // The sender half is intentionally leaked, not just held with an
        // `_`-prefixed binding: an `_`-prefixed local is still dropped at
        // the end of this function's scope (right as the built
        // `J2534Service` is returned), which would immediately close this
        // watch channel -- `rpc_subscribe_event`'s per-subscription cleanup
        // task (`tokio::select! { _ = shutdown_rx.changed() => {} ... }`)
        // would then see `changed()` resolve immediately (closed channel),
        // firing the cleanup arm and removing the subscription right after
        // it was installed. `rollback_stop_comm_pending_tests` (this
        // helper's original caller) never noticed because it never calls
        // anything that spawns a task reading `shutdown_rx`;
        // `rpc_subscribe_event_live_sender_tests` does.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::default(),
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(logical_links)),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(HashMap::new())),
            primitives: Arc::new(Mutex::new(primitives)),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A rollback whose captured `connect_generation` no longer matches the
    /// live `LogicalLinkState` (a reconnect completed since this RPC's own
    /// generation check) must NOT clear `stop_comm_pending` -- doing so
    /// would clear a new, unrelated `CoptStopcomm`'s own duplicate-call
    /// guard. `self.primitives`'s entry is still removed unconditionally,
    /// matching every other rollback site's existing behavior.
    #[tokio::test]
    async fn stale_generation_does_not_clear_stop_comm_pending() {
        let live_generation = 5;
        let stale_captured_generation = 4;
        let service = service_with_one_link(live_generation, true).await;

        service
            .rollback_stop_comm_pending(TEST_HANDLE, TEST_COP_HANDLE, stale_captured_generation)
            .await;

        let links = service.logical_links.lock().await;
        assert!(
            links.get(&TEST_HANDLE).unwrap().stop_comm_pending,
            "a stale-generation rollback must not clear a live CoptStopcomm's own guard"
        );
        drop(links);
        assert!(
            !service
                .primitives
                .lock()
                .await
                .contains_key(&TEST_COP_HANDLE),
            "self.primitives entry should still be removed unconditionally"
        );
    }

    /// A rollback whose captured `connect_generation` still matches the live
    /// `LogicalLinkState` (the common case: no reconnect happened) clears
    /// `stop_comm_pending` exactly as before this fix.
    #[tokio::test]
    async fn matching_generation_clears_stop_comm_pending() {
        let generation = 5;
        let service = service_with_one_link(generation, true).await;

        service
            .rollback_stop_comm_pending(TEST_HANDLE, TEST_COP_HANDLE, generation)
            .await;

        let links = service.logical_links.lock().await;
        assert!(
            !links.get(&TEST_HANDLE).unwrap().stop_comm_pending,
            "a matching-generation rollback should clear stop_comm_pending as before"
        );
        drop(links);
        assert!(
            !service
                .primitives
                .lock()
                .await
                .contains_key(&TEST_COP_HANDLE),
            "self.primitives entry should be removed"
        );
    }
}

/// Direct unit-level coverage of `reserve_tp20_broadcast_periodic` (Fix 2 +
/// Fix 3, Codex review, P1, PR #101, ADR-192/Phase 7 Stage 7c). A genuine
/// disconnect/reconnect (or suspension) landing DURING the earlier snapshot-
/// to-reservation window is infeasible to construct through the gRPC layer
/// with this crate's established test techniques -- the same class of
/// narrow-window infeasibility `rollback_stop_comm_pending_tests` (above)
/// already documents for its own generation gate. These tests instead call
/// the extracted helper directly with deliberately mismatched/suspended/
/// already-active `LogicalLinkState` fixtures, exercising the same guard
/// logic that would fire if the race actually happened.
#[cfg(test)]
mod reserve_tp20_broadcast_periodic_tests {
    use super::rollback_stop_comm_pending_tests::{TEST_HANDLE, service_with_one_link};
    use super::*;

    const TEST_COP_HANDLE: u32 = 200;

    /// The common case: generation matches, the CLL is connected, not
    /// suspended, and has no live broadcast periodic already -- the
    /// reservation is written and the LIVE `channel_id`/`hw_protocol_id` are
    /// returned.
    #[tokio::test]
    async fn matching_generation_connected_and_idle_reserves_and_returns_live_channel() {
        let generation = 5;
        let service = service_with_one_link(generation, false).await;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.connected = true;
            link.channel_id = Some(j2534_0404::ChannelId(42));
            link.hw_protocol_id = j2534_0404::PROTOCOL_TP2_0_PS;
        }

        let (channel_id, hw_protocol_id) = service
            .reserve_tp20_broadcast_periodic(TEST_HANDLE, TEST_COP_HANDLE, generation)
            .await
            .expect("matching generation, connected, idle CLL should reserve successfully");

        assert_eq!(channel_id, j2534_0404::ChannelId(42));
        assert_eq!(hw_protocol_id, j2534_0404::PROTOCOL_TP2_0_PS);
        let links = service.logical_links.lock().await;
        assert_eq!(
            links.get(&TEST_HANDLE).unwrap().tp20_broadcast_periodic,
            Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 0,
            }),
            "the None-sentinel reservation should be written"
        );
    }

    /// Fix 3: a reconnect landing between the RPC's early `LinkView` snapshot
    /// and this later re-check bumps `connect_generation` -- the mismatch
    /// must reject synchronously rather than reserve against a possibly
    /// different physical channel.
    #[tokio::test]
    async fn mismatched_generation_is_rejected_and_does_not_reserve() {
        let live_generation = 5;
        let snapshot_generation = 4;
        let service = service_with_one_link(live_generation, false).await;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.connected = true;
            link.channel_id = Some(j2534_0404::ChannelId(42));
        }

        let err = service
            .reserve_tp20_broadcast_periodic(TEST_HANDLE, TEST_COP_HANDLE, snapshot_generation)
            .await
            .expect_err("a stale connect_generation must be rejected");
        assert_eq!(err.code(), Code::FailedPrecondition);

        let links = service.logical_links.lock().await;
        assert_eq!(
            links.get(&TEST_HANDLE).unwrap().tp20_broadcast_periodic,
            None,
            "a rejected reservation attempt must not write the sentinel"
        );
    }

    /// Fix 3: a plain disconnect (no reconnect) leaves `connect_generation`
    /// unchanged -- `connected` must be checked too, or this case would slip
    /// through the generation check alone.
    #[tokio::test]
    async fn disconnected_with_unchanged_generation_is_rejected() {
        let generation = 5;
        let service = service_with_one_link(generation, false).await;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.connected = false;
            link.channel_id = None;
        }

        let err = service
            .reserve_tp20_broadcast_periodic(TEST_HANDLE, TEST_COP_HANDLE, generation)
            .await
            .expect_err("a disconnected CLL must be rejected even with an unchanged generation");
        assert_eq!(err.code(), Code::FailedPrecondition);
    }

    /// A suspended CLL (any of the three `tx_suspended` sources) is rejected
    /// even when connected and generation-matched.
    #[tokio::test]
    async fn suspended_cll_is_rejected_and_does_not_reserve() {
        let generation = 5;
        let service = service_with_one_link(generation, false).await;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.connected = true;
            link.channel_id = Some(j2534_0404::ChannelId(42));
            link.tx_suspended_by_ioctl = true;
        }

        let err = service
            .reserve_tp20_broadcast_periodic(TEST_HANDLE, TEST_COP_HANDLE, generation)
            .await
            .expect_err("a suspended CLL must reject a broadcast periodic start");
        assert_eq!(err.code(), Code::FailedPrecondition);

        let links = service.logical_links.lock().await;
        assert_eq!(
            links.get(&TEST_HANDLE).unwrap().tp20_broadcast_periodic,
            None,
            "a rejected reservation attempt must not write the sentinel"
        );
    }

    /// A CLL that already has a live broadcast periodic is rejected, and the
    /// existing entry is left untouched (not clobbered).
    #[tokio::test]
    async fn already_active_cll_is_rejected_and_does_not_clobber() {
        let generation = 5;
        let service = service_with_one_link(generation, false).await;
        let existing = Tp20BroadcastPeriodic {
            cop_handle: 999,
            message_id: Some(j2534_0404::PeriodicMessageId(7)),
            started_epoch: 3,
            pending_clear_generation: 0,
        };
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.connected = true;
            link.channel_id = Some(j2534_0404::ChannelId(42));
            link.tp20_broadcast_periodic = Some(existing);
        }

        let err = service
            .reserve_tp20_broadcast_periodic(TEST_HANDLE, TEST_COP_HANDLE, generation)
            .await
            .expect_err("an already-active CLL must reject a second concurrent start");
        assert_eq!(err.code(), Code::FailedPrecondition);

        let links = service.logical_links.lock().await;
        assert_eq!(
            links.get(&TEST_HANDLE).unwrap().tp20_broadcast_periodic,
            Some(existing),
            "the first COP's own tracking must not be clobbered by the rejected second start"
        );
    }

    /// A CLL that vanished from `logical_links` entirely (a rare edge, e.g. a
    /// completed `DestroyComLogicalLink` racing this call) is rejected the
    /// same way a generation mismatch is, not silently treated as idle.
    #[tokio::test]
    async fn vanished_cll_is_rejected() {
        let generation = 5;
        let service = service_with_one_link(generation, false).await;
        service.logical_links.lock().await.remove(&TEST_HANDLE);

        let err = service
            .reserve_tp20_broadcast_periodic(TEST_HANDLE, TEST_COP_HANDLE, generation)
            .await
            .expect_err("a vanished CLL must be rejected");
        assert_eq!(err.code(), Code::FailedPrecondition);
    }
}

#[cfg(test)]
mod tp20_broadcast_periodic_api_fence_tests {
    //! Direct unit-level coverage of ADR-193's `self.api`-as-serialization-
    //! fence design (Codex review round 15 Fix 2, P1, PR #101): the
    //! start-side revalidation
    //! (`revalidate_tp20_broadcast_periodic_reservation`) and the shared
    //! terminator-side take (`take_broadcast_periodic_under_api_locked`,
    //! `rpc_misc.rs`), plus the `CoptCancel` call site that wires them
    //! together.
    //!
    //! **Why direct calls rather than two genuinely concurrent RPCs.** Same
    //! established infeasibility this crate already documents for
    //! `rollback_stop_comm_pending_tests` (above in this file) and for the
    //! several PR #97/#101 concurrency fixes listed in
    //! the backlog:
    //! reproducing "a terminator lands between the reservation write and the
    //! native start" needs deterministic control over task scheduling at a
    //! specific point inside an in-flight native call, and this crate has no
    //! test-only synchronization hook for that -- its `current_thread` test
    //! runtime offers no natural preemption point in the window either.
    //! These tests therefore drive each side of the fence directly, seeding
    //! exactly the `logical_links` state the losing/winning interleavings
    //! would leave behind, and assert the resulting decision.
    use j2534_0404_sys::libloading::{Library, Symbol};
    use serial_test::serial;

    use super::rollback_stop_comm_pending_tests::{TEST_HANDLE, service_with_one_link};
    use super::*;

    const TEST_COP_HANDLE: u32 = 200;
    const OTHER_COP_HANDLE: u32 = 201;
    const TEST_GENERATION: u64 = 5;

    /// Same technique as `finalize_or_orphan_broadcast_periodic_start_tests::
    /// stop_periodic_call_count` (see that function's own doc comment for the
    /// same-loaded-shared-object rationale this relies on).
    fn stop_periodic_call_count() -> usize {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = Library::new(&lib_path).expect("mock library should be loadable");
            let f: Symbol<unsafe extern "system" fn() -> usize> = lib
                .get(b"__mock_get_stop_periodic_count_on_current_thread\0")
                .expect("__mock_get_stop_periodic_count_on_current_thread should be exported");
            f()
        }
    }

    /// Number of periodic messages currently live on `channel_id`
    /// device-side. Same same-loaded-shared-object technique as
    /// `stop_periodic_call_count` above, but keyed by a channel the calling
    /// test connected for itself, so -- unlike the process-global call
    /// counters -- it can never observe another test's periodic traffic and
    /// needs no `#[serial]` guard.
    fn live_periodic_msg_count(channel_id: ChannelId) -> usize {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = Library::new(&lib_path).expect("mock library should be loadable");
            let f: Symbol<unsafe extern "system" fn(u32) -> usize> = lib
                .get(b"__mock_get_periodic_msg_count\0")
                .expect("__mock_get_periodic_msg_count should be exported");
            f(channel_id.0)
        }
    }

    /// A connected, non-suspended CLL carrying this cop_handle's own
    /// `None`-sentinel reservation -- exactly the state
    /// `reserve_tp20_broadcast_periodic` leaves behind, and the state the
    /// start bracket revalidates against once it wins `self.api`.
    async fn service_with_a_live_reservation() -> J2534Service {
        let service = service_with_one_link(TEST_GENERATION, false).await;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.connected = true;
            link.channel_id = Some(j2534_0404::ChannelId(42));
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 0,
            });
        }
        service.primitives.lock().await.insert(
            TEST_COP_HANDLE,
            CopEntry {
                cll_handle: TEST_HANDLE,
                dispatched: false,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );
        service
    }

    async fn revalidate(service: &J2534Service) -> BroadcastPeriodicRevalidation {
        let api = service.api.lock().await;
        service
            .revalidate_tp20_broadcast_periodic_reservation(
                &api,
                TEST_HANDLE,
                TEST_COP_HANDLE,
                TEST_GENERATION,
            )
            .await
    }

    /// This CLL's `ChannelKey` for the tests below that drive the REAL
    /// `rpc_start_com_primitive` (which resolves its `tx_queue` through
    /// `shared_channels`, so a matching entry has to exist).
    const TEST_CHANNEL_KEY: ChannelKey = (j2534_0404::PROTOCOL_TP2_0_PS, 500_000, 0, 0);

    /// Mirrors `rpc_cancel_com_primitive_broadcast_periodic_session_gate_tests::
    /// shared_channel`'s own construction shape (below in this file).
    fn shared_channel(channel_id: ChannelId) -> SharedChannel {
        SharedChannel {
            channel_id,
            ref_count: 1,
            tx_queue: tokio::sync::mpsc::unbounded_channel().0,
            executing_cop: Arc::new(Mutex::new(None)),
            _poll_cancel: tokio::sync::oneshot::channel().0,
            connect_flags: 0,
            dead: false,
            occupancy_epoch: 0,
            leaked_repeat_message_ids: Vec::new(),
            leaked_periodic_message_ids: Vec::new(),
            applied_analog_sample_rate: None,
            applied_analog_samples_per_reading: None,
            applied_analog_readings_per_msg: None,
            j1939_claims: HashMap::new(),
            j1939_claim_results: HashMap::new(),
            j1939_reclaim_pending: HashMap::new(),
            leaked_j1939_claims: Vec::new(),
            tp20_connections: HashMap::new(),
            tp20_connection_results: HashMap::new(),
            become_master_in_flight: Arc::new(portable_atomic::AtomicBool::new(false)),
            tp20_passive: None,
        }
    }

    /// Everything `rpc_start_com_primitive`'s broadcast-periodic branch needs
    /// to be driven FOR REAL (Codex review round 15 follow-up, ADR-193): a
    /// connected TP2.0 CLL with `CP_TP20BroadcastAddress` staged in Active, a
    /// genuinely open mock `channel_id`, and a matching `shared_channels`
    /// entry for the `tx_queue` lookup every `CoptSendrecv` performs before
    /// the branch is reached. No reservation is seeded -- the RPC writes its
    /// own.
    async fn service_for_a_real_start() -> (J2534Service, ChannelId) {
        let service = service_with_one_link(TEST_GENERATION, false).await;
        let channel_id = connect_mock_tp20_channel(&service).await;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.connected = true;
            link.protocol = ChannelProtocol::TP2_0_PS;
            link.hw_protocol_id = j2534_0404::PROTOCOL_TP2_0_PS;
            link.channel_id = Some(channel_id);
            link.channel_key = Some(TEST_CHANNEL_KEY);
            link.active
                .unum32
                .insert(PARAM_TP20_BROADCAST_ADDRESS, 0xFF);
        }
        service
            .shared_channels
            .lock()
            .await
            .insert(TEST_CHANNEL_KEY, shared_channel(channel_id));
        // `service_with_one_link` seeds an unrelated `primitives` entry; drop
        // it so every assertion below can talk about "the" COP this RPC
        // allocates for itself.
        service.primitives.lock().await.clear();
        (service, channel_id)
    }

    /// The `StartComPrimitiveRequest` a client sends for a TP2.0 broadcast
    /// periodic re-trigger -- the same shape `tests/grpc_mock/tp20.rs`'s own
    /// `start_broadcast_periodic` builds (`num_send_cycles == -1`,
    /// `num_receive_cycles == 0`, no expected responses), minus
    /// `temp_param_update`: a `Plain` binding keeps these tests off the
    /// temp-apply/revert sub-bracket, which has its own coverage elsewhere
    /// and is irrelevant to the fence.
    fn broadcast_periodic_request() -> Request<vci_service_interface::StartComPrimitiveRequest> {
        Request::new(vci_service_interface::StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                module_handle: DEFAULT_MODULE_HANDLE,
                cll_handle: TEST_HANDLE,
            }),
            cop_type: vci_service_interface::ComOperationType::CoptSendrecv as i32,
            cop_data: vec![0x01, 0x02],
            cop_ctrl_data: Some(vci_service_interface::ComPrimitiveCtrlData {
                time: 50,
                num_send_cycles: -1,
                num_receive_cycles: 0,
                temp_param_update: 0,
                expected_response_array: Vec::new(),
                tx_flag: None,
            }),
        })
    }

    /// `broadcast_periodic_request` with `temp_param_update = 1`, for the one
    /// test that has to exercise ADR-067 claim D's Working-writeback on this
    /// branch's early `Ok` return.
    fn broadcast_periodic_request_temp_bound()
    -> Request<vci_service_interface::StartComPrimitiveRequest> {
        let mut request = broadcast_periodic_request().into_inner();
        request.cop_ctrl_data.as_mut().unwrap().temp_param_update = 1;
        Request::new(request)
    }

    /// Opens a real mock channel (`PassThruOpen`/`PassThruConnect`, plus the
    /// `CONFIG_J1962_PINS` binding SAE J2534-2 clause 6.3.3.2 requires before
    /// a `_PS` channel accepts any I/O) so the native
    /// `PassThruStartPeriodicMsg` the real RPC issues actually succeeds.
    async fn connect_mock_tp20_channel(service: &J2534Service) -> ChannelId {
        let api = service.api.lock().await;
        let device = api.open(None).expect("mock PassThruOpen should succeed");
        let channel = api
            .connect(device, j2534_0404::PROTOCOL_TP2_0_PS, 0, 500_000)
            .expect("mock PassThruConnect should succeed");
        api.set_config(channel, &[(j2534_0404::CONFIG_J1962_PINS, 0x0000_0106)])
            .expect("binding J1962 pins on a _PS channel should succeed");
        channel
    }

    /// Drives the spawned RPC task until it parks on `self.api` (which the
    /// caller is holding), i.e. until its reservation is visible in
    /// `logical_links`, and returns the `cop_handle` it allocated. Panics
    /// rather than hanging if that never happens.
    ///
    /// Deterministic on this crate's `current_thread` test runtime: the RPC
    /// reaches `self.api.lock().await` through nothing but uncontended
    /// `Mutex::lock`s (each `Poll::Ready`, so never a yield point), so the
    /// very first `yield_now` that schedules the task runs it all the way to
    /// the contended fence acquisition and no further.
    async fn wait_until_parked_on_the_fence(service: &J2534Service) -> u32 {
        for _ in 0..64 {
            tokio::task::yield_now().await;
            if let Some(periodic) = service
                .logical_links
                .lock()
                .await
                .get(&TEST_HANDLE)
                .and_then(|l| l.tp20_broadcast_periodic)
            {
                assert_eq!(
                    periodic.message_id, None,
                    "the RPC must park on the fence with its reservation still a `None` sentinel"
                );
                return periodic.cop_handle;
            }
        }
        panic!("the broadcast-periodic start never reached its reservation");
    }

    /// Every predicate still holds: the start proceeds to the temp-param
    /// apply and the native `PassThruStartPeriodicMsg` exactly as before.
    #[tokio::test]
    async fn revalidation_passes_when_every_predicate_still_holds() {
        let service = service_with_a_live_reservation().await;
        assert!(matches!(
            revalidate(&service).await,
            BroadcastPeriodicRevalidation::Ok
        ));
    }

    /// The headline case (the P1 finding): a terminator won `self.api` first
    /// and took this cop_handle's `None`-sentinel reservation. The start must
    /// abort BEFORE the native call and report the RPC successful rather than
    /// an error -- the terminator owns this cop_handle now.
    ///
    /// It must ALSO leave `self.primitives` alone (edge-case-hunter finding,
    /// round 15 follow-up, ADR-193): the terminator that took the reservation
    /// may not have reported anything yet -- it can still be queued behind
    /// `self.api` itself -- and every terminator's finalization is gated on
    /// that entry still being present. `a_start_that_loses_the_fence_leaves_
    /// primitives_for_the_terminator_to_finalize` (below) drives that whole
    /// sequence through the real RPC; this one pins the decision itself.
    #[tokio::test]
    async fn revalidation_reports_already_resolved_when_a_terminator_took_the_sentinel() {
        let service = service_with_a_live_reservation().await;
        // What a terminator's own `take_broadcast_periodic_under_api_locked`
        // leaves behind.
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_HANDLE)
            .unwrap()
            .tp20_broadcast_periodic = None;

        assert!(matches!(
            revalidate(&service).await,
            BroadcastPeriodicRevalidation::AlreadyResolved
        ));
        assert!(
            service
                .primitives
                .lock()
                .await
                .contains_key(&TEST_COP_HANDLE),
            "the aborted start must leave the `primitives` entry for the terminator that took \
             the reservation to finalize"
        );
    }

    /// A fresh reservation for a DIFFERENT cop_handle now occupies the slot
    /// (this one was taken, and a second `StartComPrimitive` legitimately
    /// reserved afterward): still "already resolved" for this cop_handle.
    ///
    /// The rollback assertion below covers the OTHER failure paths that still
    /// call `rollback_tp20_broadcast_periodic_reservation` after the fence has
    /// been won (a failed message construction, a failed temp-param apply, a
    /// failed native start): none of them re-checks ownership, so the helper
    /// itself has to stay ownership-gated or it would clobber the unrelated
    /// reservation. The `AlreadyResolved` exit itself no longer calls it at
    /// all (ADR-193, round-15 follow-up).
    #[tokio::test]
    async fn revalidation_reports_already_resolved_when_another_cop_owns_the_slot() {
        let service = service_with_a_live_reservation().await;
        let other = Tp20BroadcastPeriodic {
            cop_handle: OTHER_COP_HANDLE,
            message_id: None,
            started_epoch: 0,
            pending_clear_generation: 0,
        };
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_HANDLE)
            .unwrap()
            .tp20_broadcast_periodic = Some(other);

        assert!(matches!(
            revalidate(&service).await,
            BroadcastPeriodicRevalidation::AlreadyResolved
        ));

        service
            .rollback_tp20_broadcast_periodic_reservation(TEST_HANDLE, TEST_COP_HANDLE)
            .await;
        assert_eq!(
            service
                .logical_links
                .lock()
                .await
                .get(&TEST_HANDLE)
                .unwrap()
                .tp20_broadcast_periodic,
            Some(other),
            "another cop_handle's fresh reservation must survive this start's rollback"
        );
    }

    /// A destroyed CLL takes the reservation with it -- the same
    /// already-resolved outcome, never a panic or a native start against a
    /// channel this service no longer tracks.
    #[tokio::test]
    async fn revalidation_reports_already_resolved_when_the_cll_vanished() {
        let service = service_with_a_live_reservation().await;
        service.logical_links.lock().await.remove(&TEST_HANDLE);
        assert!(matches!(
            revalidate(&service).await,
            BroadcastPeriodicRevalidation::AlreadyResolved
        ));
    }

    /// A genuinely different predicate broke while this call was queued
    /// behind `self.api` -- the reservation is STILL this cop_handle's own,
    /// so this is a real rejection (an error), not the silent `Ok` the
    /// already-resolved case gets. The `Status` must be the exact one
    /// `reserve_tp20_broadcast_periodic`'s own initial suspension check
    /// returns.
    #[tokio::test]
    async fn revalidation_rejects_with_the_suspension_status_when_tx_became_suspended() {
        let service = service_with_a_live_reservation().await;
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_HANDLE)
            .unwrap()
            .tx_suspended_by_ioctl = true;

        let BroadcastPeriodicRevalidation::Rejected(status) = revalidate(&service).await else {
            panic!("a suspension that landed under the fence must be a rejection, not a silent Ok");
        };
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert!(
            status.message().contains("TX dispatch is suspended"),
            "unexpected rejection message: {}",
            status.message()
        );
        assert!(
            service
                .logical_links
                .lock()
                .await
                .get(&TEST_HANDLE)
                .unwrap()
                .tp20_broadcast_periodic
                .is_some(),
            "this rejection leaves the still-owned reservation in place for the caller's own \
             rollback to clear"
        );
    }

    /// A plain disconnect (no reconnect, so `connect_generation` is
    /// unchanged) that landed while this call was queued behind `self.api`
    /// is a rejection too, carrying `reserve_tp20_broadcast_periodic`'s own
    /// `PDU_ERR_CLL_NOT_CONNECTED`-flavoured status.
    #[tokio::test]
    async fn revalidation_rejects_when_the_link_disconnected_under_the_fence() {
        let service = service_with_a_live_reservation().await;
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_HANDLE)
            .unwrap()
            .connected = false;

        let BroadcastPeriodicRevalidation::Rejected(status) = revalidate(&service).await else {
            panic!("a disconnect that landed under the fence must be a rejection");
        };
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert!(
            status.message().contains("disconnected and reconnected"),
            "unexpected rejection message: {}",
            status.message()
        );
    }

    /// Terminator side, branch 1: the reservation is still this cop_handle's
    /// own `None`-sentinel, so the terminator won the fence -- the entry is
    /// taken (with the session identity captured in the same critical
    /// section) and any stale `cancelled_cops` mark is drained with it. No
    /// real id exists, so the caller issues no native stop.
    #[tokio::test]
    async fn take_under_api_takes_a_still_owned_sentinel() {
        let service = service_with_a_live_reservation().await;
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_HANDLE)
            .unwrap()
            .cancelled_cops
            .insert(TEST_COP_HANDLE);

        let api = service.api.lock().await;
        let taken = service
            .take_broadcast_periodic_under_api_locked(&api, TEST_HANDLE, TEST_COP_HANDLE)
            .await
            .expect("a still-owned sentinel must be taken");
        drop(api);

        assert_eq!(taken.periodic.cop_handle, TEST_COP_HANDLE);
        assert_eq!(taken.periodic.message_id, None);
        assert_eq!(taken.channel_id, Some(j2534_0404::ChannelId(42)));
        assert_eq!(taken.connect_generation, TEST_GENERATION);
        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_HANDLE).unwrap();
        assert_eq!(link.tp20_broadcast_periodic, None, "the slot must be taken");
        assert!(
            !link.cancelled_cops.contains(&TEST_COP_HANDLE),
            "a stale cancelled_cops mark must not outlive this COP's teardown"
        );
    }

    /// Terminator side, branch 2: the start bracket won the fence and
    /// committed a real id before this terminator could acquire `self.api`.
    /// The take reports that real id, so the caller runs its ordinary
    /// live-stop machinery instead of the sentinel skip.
    #[tokio::test]
    async fn take_under_api_returns_the_committed_id_when_the_start_won_the_fence() {
        let service = service_with_a_live_reservation().await;
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_HANDLE)
            .unwrap()
            .tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
            cop_handle: TEST_COP_HANDLE,
            message_id: Some(j2534_0404::PeriodicMessageId(77)),
            started_epoch: 9,
            pending_clear_generation: 0,
        });

        let api = service.api.lock().await;
        let taken = service
            .take_broadcast_periodic_under_api_locked(&api, TEST_HANDLE, TEST_COP_HANDLE)
            .await
            .expect("a committed entry must be taken");
        drop(api);

        assert_eq!(
            taken.periodic.message_id,
            Some(j2534_0404::PeriodicMessageId(77)),
            "the caller must see the real id, not the sentinel"
        );
        assert_eq!(taken.periodic.started_epoch, 9);
    }

    /// Terminator side, branch 3: nothing this cop_handle owns is there any
    /// more (another resolver already acted, or a different cop_handle now
    /// occupies the slot) -- a no-op that must leave the other COP's entry
    /// completely untouched.
    #[tokio::test]
    async fn take_under_api_is_a_noop_when_a_different_cop_owns_the_slot() {
        let service = service_with_a_live_reservation().await;
        let other = Tp20BroadcastPeriodic {
            cop_handle: OTHER_COP_HANDLE,
            message_id: Some(j2534_0404::PeriodicMessageId(3)),
            started_epoch: 1,
            pending_clear_generation: 0,
        };
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_HANDLE)
            .unwrap()
            .tp20_broadcast_periodic = Some(other);

        let api = service.api.lock().await;
        let taken = service
            .take_broadcast_periodic_under_api_locked(&api, TEST_HANDLE, TEST_COP_HANDLE)
            .await;
        drop(api);

        assert!(taken.is_none(), "a foreign entry must never be taken");
        assert_eq!(
            service
                .logical_links
                .lock()
                .await
                .get(&TEST_HANDLE)
                .unwrap()
                .tp20_broadcast_periodic,
            Some(other),
            "the other cop_handle's entry must be left exactly as it was"
        );
    }

    /// An absent CLL is the same no-op, never a panic.
    #[tokio::test]
    async fn take_under_api_is_a_noop_when_the_cll_is_gone() {
        let service = service_with_a_live_reservation().await;
        service.logical_links.lock().await.remove(&TEST_HANDLE);
        let api = service.api.lock().await;
        assert!(
            service
                .take_broadcast_periodic_under_api_locked(&api, TEST_HANDLE, TEST_COP_HANDLE)
                .await
                .is_none()
        );
    }

    /// The `CoptCancel` call site, sentinel branch, end to end through the
    /// real RPC: the reservation is taken under the fence, no native
    /// `PassThruStopPeriodicMsg` is issued (there is no real id yet), and the
    /// COP is reported `PduCopstCancelled` immediately. The companion
    /// committed-id branch is covered by
    /// `rpc_cancel_com_primitive_zero_message_id_tests::
    /// cancel_stops_a_committed_message_whose_adapter_assigned_id_is_zero`.
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn cancel_takes_the_sentinel_under_the_fence_without_a_native_stop() {
        let service = service_with_a_live_reservation().await;
        let before = stop_periodic_call_count();

        service
            .rpc_cancel_com_primitive(Request::new(
                vci_service_interface::CancelComPrimitiveRequest {
                    cop_handle: Some(vci_service_interface::ComPrimitiveHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle: TEST_HANDLE,
                        cop_handle: TEST_COP_HANDLE,
                    }),
                },
            ))
            .await
            .expect("CancelComPrimitive should succeed against an in-flight reservation");

        assert_eq!(
            stop_periodic_call_count(),
            before,
            "a `None`-sentinel reservation has no real id to stop natively"
        );
        assert!(
            !service
                .primitives
                .lock()
                .await
                .contains_key(&TEST_COP_HANDLE),
            "the cancelled COP must be removed from `primitives`"
        );
        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_HANDLE).unwrap();
        assert_eq!(
            link.tp20_broadcast_periodic, None,
            "the reservation must be taken by the cancel"
        );
        let queue = link.rx_buf.lock().await;
        assert!(
            matches!(
                queue.items.front(),
                Some(CllQueueItem::Status(TrackedStatus {
                    event: StatusEvent::Cop {
                        cop_handle: TEST_COP_HANDLE,
                        status: vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled,
                        ..
                    },
                    ..
                }))
            ),
            "the cancel must still be client-visible immediately"
        );
    }

    /// **The round-15 regression, end to end through the real RPC**
    /// (edge-case-hunter finding, ADR-193 follow-up). A suspension-style
    /// terminator takes the `None`-sentinel entry under `logical_links`
    /// ALONE -- exactly what `ioctl_suspend_tx_queue` does in the same
    /// critical section that sets the flag -- while an in-flight
    /// `rpc_start_com_primitive` is parked on `self.api`. The start then wins
    /// the fence, revalidates, finds its reservation gone
    /// (`AlreadyResolved`), and returns a silent `Ok`.
    ///
    /// The COP it abandoned there must still be finalizable: before this fix
    /// the start also dropped the `primitives` entry (via
    /// `rollback_tp20_broadcast_periodic_reservation`), and the terminator's
    /// own later `events::emit_terminal_if_live` -- gated on exactly that
    /// entry -- then emitted NOTHING, leaving no COP status and no
    /// `terminal_cops` record, so a later `GetStatus`/`CancelComPrimitive`
    /// wrongly reported `PDU_ERR_INVALID_HANDLE` instead of ADR-128's
    /// already-terminal no-op.
    #[tokio::test]
    async fn a_start_that_loses_the_fence_leaves_primitives_for_the_terminator_to_finalize() {
        let (service, channel_id) = service_for_a_real_start().await;

        // The start bracket cannot progress past `self.api` while this test
        // task holds it -- the same fence a poll-task native call would hold.
        let api = service.api.lock().await;
        let start = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .rpc_start_com_primitive(broadcast_periodic_request())
                    .await
            }
        });
        let cop_handle = wait_until_parked_on_the_fence(&service).await;

        // `ioctl_suspend_tx_queue`'s own critical section, in shape: the flag
        // is set and the entry taken under `logical_links` ALONE, with no
        // fence at all. That unfenced take is deliberate and safe (it only
        // touches `logical_links`, never `primitives`); it is what makes the
        // racing start's own revalidation report `AlreadyResolved`.
        let (periodic, taken_channel_id, connect_generation, channel_key) = {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.tx_suspended_by_ioctl = true;
            (
                link.tp20_broadcast_periodic.take().unwrap(),
                link.channel_id,
                link.connect_generation,
                link.channel_key,
            )
        };
        drop(api);

        let response = start
            .await
            .expect("the start task must not panic")
            .expect("a start whose reservation was already taken reports a silent Ok");
        assert_eq!(
            response.get_ref().cop_handle.unwrap().cop_handle,
            cop_handle
        );
        assert_eq!(
            live_periodic_msg_count(channel_id),
            0,
            "no native PassThruStartPeriodicMsg may run once the reservation is gone"
        );
        assert!(
            service.primitives.lock().await.contains_key(&cop_handle),
            "the losing start must leave the `primitives` entry for the terminator that took \
             the reservation -- removing it makes the COP vanish with no terminal status at all"
        );
        assert!(
            service
                .terminal_cops
                .lock()
                .await
                .lookup(cop_handle)
                .is_none(),
            "nothing has reported this COP terminal yet -- the terminator was still queued \
             behind the fence"
        );

        // The terminator finally gets `self.api` (this is where it was
        // queued all along) and finalizes the COP it took.
        service
            .terminate_tp20_broadcast_periodic_for_suspension(
                TEST_HANDLE,
                periodic,
                taken_channel_id,
                connect_generation,
                channel_key,
            )
            .await;

        assert!(
            !service.primitives.lock().await.contains_key(&cop_handle),
            "the terminator must be the one that finalizes this COP"
        );
        assert_eq!(
            service.terminal_cops.lock().await.lookup(cop_handle),
            Some((
                TEST_HANDLE,
                vci_service_interface::PduComPrimitiveStatus::PduCopstFinished
            )),
            "a suspension termination reports PduCopstFinished for the COP it took"
        );
        let links = service.logical_links.lock().await;
        let queue = links.get(&TEST_HANDLE).unwrap().rx_buf.lock().await;
        assert!(
            matches!(
                queue.items.front(),
                Some(CllQueueItem::Status(TrackedStatus {
                    event: StatusEvent::Cop {
                        cop_handle: emitted,
                        status: vci_service_interface::PduComPrimitiveStatus::PduCopstFinished,
                        ..
                    },
                    ..
                })) if *emitted == cop_handle
            ),
            "the COP's terminal status must still reach the client"
        );
    }

    /// Fence-acquisition isolation for the terminator side (edge-case-hunter
    /// finding, ADR-193 follow-up): deleting
    /// `terminate_tp20_broadcast_periodic_for_suspension`'s own unconditional
    /// `self.api.lock().await` -- the whole point of round 15's change to
    /// that function, since a `None`-sentinel needs no native call -- must
    /// break a test. It breaks this one: while this test task holds the
    /// fence (standing in for an in-flight start bracket), the spawned
    /// termination must make no observable progress at all. Without the
    /// acquisition it would sail through and report the COP terminal while
    /// the start's native `PassThruStartPeriodicMsg` was still in flight,
    /// which is exactly the P1 ADR-193 closes.
    ///
    /// The blocked-vs-progressed distinction is deterministic on this
    /// crate's `current_thread` test runtime: every other await in that
    /// function is an uncontended `Mutex::lock` (always `Poll::Ready`), so
    /// yielding repeatedly runs the spawned task to completion unless the
    /// contended fence genuinely parks it.
    #[tokio::test]
    async fn a_suspension_termination_waits_for_the_fence_before_reporting_the_cop_terminal() {
        let service = service_with_a_live_reservation().await;
        let api = service.api.lock().await;

        let (periodic, channel_id, connect_generation, channel_key) = {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.tx_suspended_by_ioctl = true;
            (
                link.tp20_broadcast_periodic.take().unwrap(),
                link.channel_id,
                link.connect_generation,
                link.channel_key,
            )
        };
        let terminator = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .terminate_tp20_broadcast_periodic_for_suspension(
                        TEST_HANDLE,
                        periodic,
                        channel_id,
                        connect_generation,
                        channel_key,
                    )
                    .await;
            }
        });

        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !terminator.is_finished(),
            "the termination must be parked on `self.api`, not racing an in-flight start"
        );
        assert!(
            service
                .primitives
                .lock()
                .await
                .contains_key(&TEST_COP_HANDLE),
            "nothing may be finalized while the fence is held elsewhere"
        );
        assert!(
            service
                .terminal_cops
                .lock()
                .await
                .lookup(TEST_COP_HANDLE)
                .is_none(),
            "no terminal status may be reported while the fence is held elsewhere"
        );

        drop(api);
        terminator.await.expect("the termination must not panic");
        assert!(
            !service
                .primitives
                .lock()
                .await
                .contains_key(&TEST_COP_HANDLE),
            "once the fence is released the termination finalizes the COP as before"
        );
        assert_eq!(
            service.terminal_cops.lock().await.lookup(TEST_COP_HANDLE),
            Some((
                TEST_HANDLE,
                vci_service_interface::PduComPrimitiveStatus::PduCopstFinished
            ))
        );
    }

    /// The same fence-acquisition isolation for `CoptCancel`'s own call site
    /// (`rpc_cancel_com_primitive`, this file): its
    /// `let api = self.api.lock().await;` immediately before
    /// `take_broadcast_periodic_under_api_locked` is load-bearing, and this
    /// test fails if it is removed -- the sentinel would be taken, and
    /// `PduCopstCancelled` reported, while an in-flight start bracket (this
    /// test task, holding the fence) could still be about to transmit.
    #[tokio::test]
    async fn cancel_waits_for_the_fence_before_taking_the_sentinel() {
        let service = service_with_a_live_reservation().await;
        let api = service.api.lock().await;

        let cancel = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .rpc_cancel_com_primitive(Request::new(
                        vci_service_interface::CancelComPrimitiveRequest {
                            cop_handle: Some(vci_service_interface::ComPrimitiveHandle {
                                module_handle: DEFAULT_MODULE_HANDLE,
                                cll_handle: TEST_HANDLE,
                                cop_handle: TEST_COP_HANDLE,
                            }),
                        },
                    ))
                    .await
            }
        });

        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !cancel.is_finished(),
            "CoptCancel must be parked on `self.api` before it may touch the reservation"
        );
        assert!(
            service
                .logical_links
                .lock()
                .await
                .get(&TEST_HANDLE)
                .unwrap()
                .tp20_broadcast_periodic
                .is_some(),
            "the sentinel must not be taken until the fence is won"
        );
        assert!(
            service
                .terminal_cops
                .lock()
                .await
                .lookup(TEST_COP_HANDLE)
                .is_none(),
            "the cancel must not be reported while the fence is held elsewhere"
        );

        drop(api);
        cancel
            .await
            .expect("the cancel task must not panic")
            .expect("CancelComPrimitive should succeed against an in-flight reservation");
        assert_eq!(
            service
                .logical_links
                .lock()
                .await
                .get(&TEST_HANDLE)
                .unwrap()
                .tp20_broadcast_periodic,
            None,
            "once the fence is released the cancel takes the reservation as before"
        );
        assert_eq!(
            service.terminal_cops.lock().await.lookup(TEST_COP_HANDLE),
            Some((
                TEST_HANDLE,
                vci_service_interface::PduComPrimitiveStatus::PduCopstCancelled
            ))
        );
    }

    /// The PRODUCTION start bracket, driven through the real RPC (Codex
    /// review round 15 follow-up, ADR-193): `rpc_start_com_primitive`'s own
    /// broadcast-periodic branch must win `self.api` BEFORE it issues the
    /// native `PassThruStartPeriodicMsg`, and the entry the next fence holder
    /// observes afterwards must be the committed real `message_id`, never the
    /// `None` sentinel.
    ///
    /// This is the production-path companion to
    /// `commit_happens_before_the_api_guard_is_released` below, which drives
    /// the resolution helper directly. Neither can prove the STRICT ordering
    /// "the commit lands before `drop(api)`" through a real RPC: observing
    /// that would need another task to run inside the guard's own window, and
    /// this crate's `current_thread` test runtime offers no preemption point
    /// there (the module header's standing infeasibility, also recorded in
    /// the backlog). What this
    /// test does pin, deterministically, is that no native start happens on
    /// the wrong side of the fence -- it fails if the bracket's
    /// `self.api.lock().await` is dropped or moved after the native call.
    #[tokio::test]
    async fn the_real_start_bracket_holds_the_fence_across_the_native_start() {
        let (service, channel_id) = service_for_a_real_start().await;
        assert_eq!(live_periodic_msg_count(channel_id), 0);

        let api = service.api.lock().await;
        let start = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .rpc_start_com_primitive(broadcast_periodic_request())
                    .await
            }
        });
        let cop_handle = wait_until_parked_on_the_fence(&service).await;
        assert_eq!(
            live_periodic_msg_count(channel_id),
            0,
            "the native PassThruStartPeriodicMsg must not run before the fence is won"
        );
        assert!(!start.is_finished());

        drop(api);
        start
            .await
            .expect("the start task must not panic")
            .expect("a broadcast-periodic CoptSendrecv should be accepted");
        assert_eq!(
            live_periodic_msg_count(channel_id),
            1,
            "the native periodic message must be live once the bracket completed"
        );
        assert!(
            service
                .primitives
                .lock()
                .await
                .get(&cop_handle)
                .expect("the started COP stays live in `primitives`")
                .dispatched,
            "the post-`drop(api)` bookkeeping half must still run"
        );

        // The very first terminator able to acquire the fence afterwards sees
        // a committed id it can genuinely stop -- never the sentinel.
        let api = service.api.lock().await;
        let taken = service
            .take_broadcast_periodic_under_api_locked(&api, TEST_HANDLE, cop_handle)
            .await
            .expect("the terminator must find the committed entry");
        let message_id = taken
            .periodic
            .message_id
            .expect("the entry the next fence holder sees must carry the real message id");
        // Housekeeping: leave no live periodic message behind on the mock's
        // process-global channel state.
        let _ = api.stop_periodic_message(channel_id, message_id);
        drop(api);
    }

    /// The reordering round 15 introduced, asserted directly (see this
    /// module's own header for why a truly concurrent interleaving is not
    /// constructible here): the start bracket's resolution commits the real
    /// `message_id` into `logical_links` while `self.api` is STILL held, so
    /// the very first terminator able to acquire that guard afterwards
    /// observes a committed id it can genuinely stop -- never the
    /// `None`-sentinel it would have had to skip (reporting the COP terminal
    /// while the message kept transmitting) had the commit still happened
    /// after `drop(api)`.
    ///
    /// Deliberately a DIRECT call to the locked half: it pins that helper's
    /// own contract (it commits, rather than merely deciding to commit, while
    /// the guard is held). Coverage of the real caller that must invoke it
    /// inside its own guard lives in
    /// `the_real_start_bracket_holds_the_fence_across_the_native_start`
    /// above, which drives `rpc_start_com_primitive` end to end.
    #[tokio::test]
    async fn commit_happens_before_the_api_guard_is_released() {
        let service = service_with_a_live_reservation().await;
        let channel_id = j2534_0404::ChannelId(42);

        let api = service.api.lock().await;
        let resolution = service
            .finalize_or_orphan_broadcast_periodic_start_locked(
                &api,
                TEST_HANDLE,
                TEST_COP_HANDLE,
                channel_id,
                j2534_0404::PeriodicMessageId(77),
                7,
            )
            .await;
        assert!(matches!(
            resolution,
            BroadcastPeriodicStartResolution::Live { .. }
        ));
        // Still holding the fence: the commit is already visible.
        assert_eq!(
            service
                .logical_links
                .lock()
                .await
                .get(&TEST_HANDLE)
                .unwrap()
                .tp20_broadcast_periodic,
            Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: Some(j2534_0404::PeriodicMessageId(77)),
                started_epoch: 7,
                pending_clear_generation: 0,
            }),
            "the real message id must be committed BEFORE `self.api` is released"
        );
        drop(api);

        // A terminator acquiring the fence right afterwards sees the real id.
        let api = service.api.lock().await;
        let taken = service
            .take_broadcast_periodic_under_api_locked(&api, TEST_HANDLE, TEST_COP_HANDLE)
            .await
            .expect("the terminator must find the committed entry");
        assert_eq!(
            taken.periodic.message_id,
            Some(j2534_0404::PeriodicMessageId(77)),
            "the terminator must be able to stop the real message rather than skipping a sentinel"
        );
    }

    /// ADR-067 claim D's `temp_param_update` Working-writeback must run on
    /// this branch's EARLY `Ok` return -- the
    /// `BroadcastPeriodicRevalidation::AlreadyResolved` arm -- exactly as it
    /// does on `rpc_start_com_primitive`'s ordinary `Ok` tail
    /// (edge-case-hunter finding, round 15 follow-up, ADR-193 Consequences).
    ///
    /// The criterion is acceptance, not hardware consumption: this arm
    /// returns `Ok` for a COP that was really registered in `primitives`
    /// (and handed to the terminator that took the reservation), so it is a
    /// successful call, not a rejected one. Skipping the writeback left the
    /// client's staged Working values in place after a call it was told
    /// succeeded -- observable, since a later `CoptUpdateparam` would promote
    /// values that different race timing would have discarded.
    #[tokio::test]
    async fn a_start_that_loses_the_fence_still_performs_the_temp_param_writeback() {
        let (service, channel_id) = service_for_a_real_start().await;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.working = link.active.clone();
            // A non-BUSTYPE/non-TESTER_PRESENT staged difference, so ADR-067
            // claim E's guard does not reject the call before the branch.
            link.working
                .unum32
                .insert(PARAM_TP20_BROADCAST_ADDRESS, 0xFE);
        }

        let api = service.api.lock().await;
        let start = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .rpc_start_com_primitive(broadcast_periodic_request_temp_bound())
                    .await
            }
        });
        let cop_handle = wait_until_parked_on_the_fence(&service).await;
        // `ioctl_suspend_tx_queue`'s unfenced take, in shape (see
        // `a_start_that_loses_the_fence_leaves_primitives_for_the_terminator_
        // to_finalize` above for the same move and why it is safe).
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_HANDLE)
            .unwrap()
            .tp20_broadcast_periodic = None;
        drop(api);

        let response = start
            .await
            .expect("the start task must not panic")
            .expect("a start whose reservation was already taken reports a silent Ok");
        assert_eq!(
            response.get_ref().cop_handle.unwrap().cop_handle,
            cop_handle
        );
        assert_eq!(
            live_periodic_msg_count(channel_id),
            0,
            "no native start may run once the reservation is gone -- so the staged Working \
             snapshot was never applied to hardware, which is deliberately NOT the criterion"
        );

        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_HANDLE).unwrap();
        assert_eq!(
            link.working.unum32.get(&PARAM_TP20_BROADCAST_ADDRESS),
            Some(&0xFF),
            "ADR-067 claim D's Working-writeback must have reset Working to Active on this \
             `Ok` return, exactly as the RPC's ordinary `Ok` tail does"
        );
        assert_eq!(link.working.unum32, link.active.unum32);
    }

    /// The PRODUCTION `DisconnectComLogicalLink` teardown, driven through the
    /// real RPC against a real, natively-live broadcast periodic message
    /// (edge-case-hunter finding, round 15 follow-up, ADR-193): the companion
    /// to `cancel_waits_for_the_fence_before_taking_the_sentinel` above, for
    /// the one terminator whose take is deliberately NOT inside the fence.
    ///
    /// Two properties, and they pull in opposite directions -- which is
    /// exactly why this test exists:
    ///
    /// 1. **The take is ATOMIC with the session clear.** `Disconnect` clears
    ///    `channel_id`/`channel_key`/`connected` and takes
    ///    `tp20_broadcast_periodic` in ONE `logical_links` critical section,
    ///    before it ever queues for `self.api`. While it is parked on the
    ///    fence, no observer reading `logical_links` alone -- which is all
    ///    `ioctl_suspend_tx_queue`, the `CP_SuspendQueueOnError` sites in
    ///    `events.rs`, and `CoptCancel`'s own session capture ever hold --
    ///    may see a live entry paired with an already-cleared `channel_id`.
    ///    Such an observer would take the entry, capture `channel_id: None`,
    ///    skip its native stop for want of a channel, and report its COP
    ///    terminal, leaving a still-transmitting periodic message with zero
    ///    tracking anywhere. Round 15 briefly moved the take into the fenced
    ///    section and opened precisely that window; this assertion is the
    ///    regression guard.
    /// 2. **The native-stop DECISION is still FENCED.** The message stays
    ///    live device-side until `self.api` is released, so the decision can
    ///    never race an in-flight `PassThruStartPeriodicMsg`.
    ///
    /// Joins the `tp20_stop_periodic_call_counter` serial group: the teardown
    /// issues a real `PassThruStopPeriodicMsg`, which bumps the mock's
    /// PROCESS-GLOBAL `__mock_get_stop_periodic_count` that
    /// `cancel_takes_the_sentinel_under_the_fence_without_a_native_stop`
    /// asserts on. (This test's own assertions use `live_periodic_msg_count`,
    /// which is channel-keyed and would need no guard by itself.)
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn disconnect_takes_the_entry_atomically_and_fences_only_the_native_stop() {
        let (service, channel_id) = service_for_a_real_start().await;
        service
            .rpc_start_com_primitive(broadcast_periodic_request())
            .await
            .expect("a broadcast-periodic CoptSendrecv should be accepted");
        assert_eq!(
            live_periodic_msg_count(channel_id),
            1,
            "the teardown below needs a genuinely live native periodic message"
        );
        // Keep the physical channel alive past this CLL's departure, so the
        // teardown's own `PassThruStopPeriodicMsg` is what stops the message
        // rather than the `PassThruDisconnect` a ref_count of 0 would trigger.
        service
            .shared_channels
            .lock()
            .await
            .get_mut(&TEST_CHANNEL_KEY)
            .expect("the fixture's shared channel")
            .ref_count = 2;

        let api = service.api.lock().await;
        let disconnect = tokio::spawn({
            let service = service.clone();
            async move {
                service
                    .rpc_disconnect_com_logical_link(Request::new(
                        vci_service_interface::DisconnectComLogicalLinkRequest {
                            cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                                module_handle: DEFAULT_MODULE_HANDLE,
                                cll_handle: TEST_HANDLE,
                            }),
                        },
                    ))
                    .await
            }
        });

        // Sample the CLL repeatedly across the whole parked window, exactly
        // the way a `logical_links`-only terminator would.
        for _ in 0..64 {
            tokio::task::yield_now().await;
            let links = service.logical_links.lock().await;
            let link = links
                .get(&TEST_HANDLE)
                .expect("Disconnect leaves the CLL in the map (only Destroy removes it)");
            assert!(
                link.tp20_broadcast_periodic.is_none()
                    || (link.channel_id.is_some() && link.channel_key.is_some()),
                "a tracked broadcast periodic must never be observable alongside a cleared \
                 channel_id/channel_key: a terminator reading under `logical_links` alone would \
                 capture `channel_id: None`, skip its native stop, and orphan a live periodic \
                 message (ADR-193)"
            );
        }

        assert!(
            !disconnect.is_finished(),
            "the teardown must be parked on `self.api` before its native stop"
        );
        {
            let links = service.logical_links.lock().await;
            let link = links.get(&TEST_HANDLE).unwrap();
            assert!(
                link.tp20_broadcast_periodic.is_none(),
                "the entry is taken atomically with the session clear, not late under the fence"
            );
            assert!(link.channel_id.is_none() && link.channel_key.is_none());
        }
        assert_eq!(
            live_periodic_msg_count(channel_id),
            1,
            "the native PassThruStopPeriodicMsg decision must wait for the fence"
        );

        drop(api);
        disconnect
            .await
            .expect("the teardown task must not panic")
            .expect("DisconnectComLogicalLink should succeed");
        assert_eq!(
            live_periodic_msg_count(channel_id),
            0,
            "once the fence is released the teardown stops the real message"
        );
        assert!(
            service
                .shared_channels
                .lock()
                .await
                .get(&TEST_CHANNEL_KEY)
                .expect("the surviving sibling keeps the channel open")
                .leaked_periodic_message_ids
                .is_empty(),
            "a successful stop leaves nothing to leak-track"
        );
    }
}

/// Direct unit-level coverage of
/// `finalize_or_orphan_broadcast_periodic_start_locked` and its
/// post-`drop(api)` bookkeeping half (Codex review, PR #101, round 5; split
/// into the pair by round 15's ADR-193 serialization fix): whether a just-succeeded native
/// `start_periodic_message` call commits normally or discovers its
/// `None`-sentinel reservation was already taken/cleared by a concurrent
/// CoptCancel/suspension-termination/`CLEAR_PERIODIC_MSGS`/teardown.
#[cfg(test)]
mod finalize_or_orphan_broadcast_periodic_start_tests {
    use j2534_0404_sys::libloading::{Library, Symbol};
    use serial_test::serial;

    use super::rollback_stop_comm_pending_tests::{TEST_HANDLE, service_with_one_link};
    use super::*;

    /// Reads the mock's per-thread `PassThruStopPeriodicMsg` call counter
    /// through a fresh `libloading::Library` handle, not a statically-linked
    /// copy -- mirrors `discovery.rs::tests::get_device_info_call_count`'s
    /// own documented rationale: a fresh `Library::new` on the identical path
    /// resolves to the SAME dynamically-loaded shared object `service.api`
    /// itself mutates, not a separate statically-linked copy of `MockState`.
    /// Per-thread for the same reason as that function: the process-wide
    /// count also moves whenever another test in this binary stops a
    /// periodic message in parallel, which broke the exact deltas below.
    fn stop_periodic_call_count() -> usize {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = Library::new(&lib_path).expect("mock library should be loadable");
            let f: Symbol<unsafe extern "system" fn() -> usize> = lib
                .get(b"__mock_get_stop_periodic_count_on_current_thread\0")
                .expect("__mock_get_stop_periodic_count_on_current_thread should be exported");
            f()
        }
    }

    const TEST_COP_HANDLE: u32 = 200;

    /// Runs the split start-completion pair exactly the way
    /// `rpc_start_com_primitive`'s own broadcast-periodic bracket runs it
    /// (Codex review round 15 Fix 2, P1, PR #101, ADR-193): the
    /// `logical_links` resolution -- and, for the `NotOwned` resolution, the
    /// orphan `stop_periodic_message` -- under a held `self.api` guard, then
    /// the bookkeeping half after that guard is dropped. Every test below
    /// goes through this helper rather than calling either half directly, so
    /// none of them can accidentally assert against an ordering the real
    /// caller does not use.
    async fn finalize_via_start_bracket(
        service: &J2534Service,
        handle: u32,
        cop_handle: u32,
        channel_id: j2534_0404::ChannelId,
        message_id: j2534_0404::PeriodicMessageId,
        started_epoch: u64,
    ) {
        let api = service.api.lock().await;
        let resolution = service
            .finalize_or_orphan_broadcast_periodic_start_locked(
                &api,
                handle,
                cop_handle,
                channel_id,
                message_id,
                started_epoch,
            )
            .await;
        drop(api);
        service
            .finalize_broadcast_periodic_start_bookkeeping(
                handle, cop_handle, message_id, resolution,
            )
            .await;
    }

    /// The common case: the `None`-sentinel reservation is still this exact
    /// cop_handle's own -- the real `message_id` is committed, the COP is
    /// marked `dispatched`, a single `PduCopstExecuting` status is queued,
    /// and the caller-supplied `started_epoch` round-trips unchanged into
    /// the committed entry (periodic-clear-epoch fix, Codex review round 5,
    /// PR #101, ADR-192/Phase 7 Stage 7c).
    #[tokio::test]
    async fn commits_and_reports_executing_when_reservation_still_owned() {
        let service = service_with_one_link(1, false).await;
        let channel_id = j2534_0404::ChannelId(42);
        const TEST_STARTED_EPOCH: u64 = 42;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.channel_id = Some(channel_id);
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 0,
            });
        }
        // `service_with_one_link` only seeds `self.primitives` at its own
        // (different) COP_HANDLE constant -- this module's own
        // TEST_COP_HANDLE needs its own entry for the `dispatched` assertion
        // below.
        service.primitives.lock().await.insert(
            TEST_COP_HANDLE,
            CopEntry {
                cll_handle: TEST_HANDLE,
                dispatched: false,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        finalize_via_start_bracket(
            &service,
            TEST_HANDLE,
            TEST_COP_HANDLE,
            channel_id,
            j2534_0404::PeriodicMessageId(77),
            TEST_STARTED_EPOCH,
        )
        .await;

        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_HANDLE).unwrap();
        assert_eq!(
            link.tp20_broadcast_periodic,
            Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: Some(j2534_0404::PeriodicMessageId(77)),
                started_epoch: TEST_STARTED_EPOCH,
                pending_clear_generation: 0,
            }),
            "the sentinel should be overwritten with the real message id, and the caller-\
             supplied started_epoch must round-trip unchanged into the committed entry"
        );
        assert!(
            service
                .primitives
                .lock()
                .await
                .get(&TEST_COP_HANDLE)
                .unwrap()
                .dispatched,
            "the COP must be marked dispatched"
        );
        let queue = link.rx_buf.lock().await;
        assert_eq!(
            queue.items.len(),
            1,
            "exactly one status event must be queued"
        );
        assert!(
            matches!(
                queue.items.front(),
                Some(CllQueueItem::Status(TrackedStatus {
                    event: StatusEvent::Cop {
                        cop_handle: TEST_COP_HANDLE,
                        status: vci_service_interface::PduComPrimitiveStatus::PduCopstExecuting,
                        ..
                    },
                    ..
                }))
            ),
            "the queued event must be this cop_handle's own PduCopstExecuting"
        );
    }

    /// Codex review fix (P2, PR #101, round 12, ADR-192): the `Live`
    /// resolution path commits the real entry into `logical_links` (under
    /// that lock) before this method ever reports `PduCopstExecuting`. This
    /// simulates the race a concurrent `CLEAR_PERIODIC_MSGS`/suspension-
    /// termination/`CoptCancel` can win in the gap between that commit and
    /// this method's own status emission: `primitives` has ALREADY lost
    /// `TEST_COP_HANDLE` (as a racing terminal-finalization site would leave
    /// it) even though the `logical_links` sentinel is still this exact
    /// cop_handle's own and resolves `Live`. Before the
    /// `emit_nonterminal_if_live` fix, this would have unconditionally
    /// queued a bogus `PduCopstExecuting` for an already-finalized COP; the
    /// fix's `primitives`-containment gate must suppress it.
    #[tokio::test]
    async fn resolution_live_suppresses_executing_when_primitives_entry_already_gone() {
        let service = service_with_one_link(1, false).await;
        let channel_id = j2534_0404::ChannelId(42);
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.channel_id = Some(channel_id);
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 0,
            });
        }
        // Deliberately do NOT insert a `primitives` entry for
        // TEST_COP_HANDLE -- simulates a concurrent terminal-finalization
        // site (e.g. `CLEAR_PERIODIC_MSGS`'s scan, `CoptCancel`, suspension
        // termination) having already removed it between the
        // `logical_links` commit above and this call.

        finalize_via_start_bracket(
            &service,
            TEST_HANDLE,
            TEST_COP_HANDLE,
            channel_id,
            j2534_0404::PeriodicMessageId(77),
            42,
        )
        .await;

        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_HANDLE).unwrap();
        assert_eq!(
            link.tp20_broadcast_periodic,
            Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: Some(j2534_0404::PeriodicMessageId(77)),
                started_epoch: 42,
                pending_clear_generation: 0,
            }),
            "the Live resolution still commits the real entry into logical_links regardless of \
             primitives -- only the status emission is gated"
        );
        assert!(
            !service
                .primitives
                .lock()
                .await
                .contains_key(&TEST_COP_HANDLE),
            "the entry must remain absent from primitives -- emit_nonterminal_if_live must not \
             resurrect it"
        );
        assert!(
            link.rx_buf.lock().await.items.is_empty(),
            "no PduCopstExecuting may be queued once primitives no longer contains this \
             cop_handle -- a racing terminal finalizer already reported this COP terminal"
        );
    }

    /// Codex review fix (P1/P2, PR #101, round 5): if something else
    /// (CoptCancel, a suspension termination, `CLEAR_PERIODIC_MSGS`, or CLL
    /// teardown) already took/cleared the `None`-sentinel reservation while the
    /// native start was in flight, this method must NOT resurrect tracking,
    /// must NOT mark the COP dispatched, and -- the actual regression this
    /// fix closes -- must NOT queue a `PduCopstExecuting` status for a
    /// cop_handle the losing side already reported (or is about to report)
    /// terminal.
    #[tokio::test]
    async fn does_not_report_executing_when_reservation_was_lost() {
        let service = service_with_one_link(1, false).await;
        let channel_id = j2534_0404::ChannelId(42);
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.channel_id = Some(channel_id);
            // Simulates a concurrent CoptCancel/teardown that already took
            // the `None`-sentinel reservation while the native call above was
            // in flight -- nothing left to find here.
            link.tp20_broadcast_periodic = None;
        }
        // `service_with_one_link` only seeds `self.primitives` at its own
        // (different) COP_HANDLE constant -- this module's own
        // TEST_COP_HANDLE needs its own entry for the `dispatched` assertion
        // below.
        service.primitives.lock().await.insert(
            TEST_COP_HANDLE,
            CopEntry {
                cll_handle: TEST_HANDLE,
                dispatched: false,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        finalize_via_start_bracket(
            &service,
            TEST_HANDLE,
            TEST_COP_HANDLE,
            channel_id,
            j2534_0404::PeriodicMessageId(77),
            42,
        )
        .await;

        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_HANDLE).unwrap();
        assert_eq!(
            link.tp20_broadcast_periodic, None,
            "a lost reservation must not be resurrected"
        );
        assert!(
            !service
                .primitives
                .lock()
                .await
                .get(&TEST_COP_HANDLE)
                .unwrap()
                .dispatched,
            "a COP this dispatch no longer owns must not be marked dispatched"
        );
        assert!(
            link.rx_buf.lock().await.items.is_empty(),
            "no status event -- in particular no bogus PduCopstExecuting -- may be queued for \
             a cop_handle whose reservation was already lost to a concurrent terminal event"
        );
    }

    /// The same "reservation lost" outcome when the entry still exists but
    /// belongs to a DIFFERENT cop_handle (e.g. a fresh reservation raced in
    /// for a new COP after this one was cancelled) -- not just the "entry
    /// removed entirely" case above.
    #[tokio::test]
    async fn does_not_report_executing_when_reservation_belongs_to_another_cop() {
        let service = service_with_one_link(1, false).await;
        let channel_id = j2534_0404::ChannelId(42);
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.channel_id = Some(channel_id);
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE + 1,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 0,
            });
        }

        finalize_via_start_bracket(
            &service,
            TEST_HANDLE,
            TEST_COP_HANDLE,
            channel_id,
            j2534_0404::PeriodicMessageId(77),
            42,
        )
        .await;

        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_HANDLE).unwrap();
        assert_eq!(
            link.tp20_broadcast_periodic,
            Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE + 1,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 0,
            }),
            "the other cop_handle's own reservation must not be clobbered"
        );
        assert!(
            link.rx_buf.lock().await.items.is_empty(),
            "no status event may be queued for a cop_handle whose reservation was lost, even \
             when a different cop_handle's own reservation now occupies the slot"
        );
    }

    /// design-advisor consult item 4 (Codex review round 5, PR #101,
    /// ADR-192/Phase 7 Stage 7c): the `started_epoch` a caller passes in is
    /// exactly `J2534Service::periodic_clear_epoch`'s current value at the
    /// moment of the native start's success -- mirroring
    /// `rpc_start_com_primitive`'s own broadcast-periodic branch, which
    /// reads `periodic_clear_epoch` immediately after the native
    /// `start_periodic_message` call returns `Ok`, still holding `self.api`.
    /// This test drives that same read-then-pass shape directly, without
    /// the native call, and asserts the value round-trips into the
    /// committed entry unchanged.
    #[tokio::test]
    async fn commits_captures_current_periodic_clear_epoch_value() {
        let service = service_with_one_link(1, false).await;
        let channel_id = j2534_0404::ChannelId(42);
        const N: u64 = 7;
        service
            .periodic_clear_epoch
            .store(N, portable_atomic::Ordering::Relaxed);
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.channel_id = Some(channel_id);
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 0,
            });
        }

        let started_epoch = service
            .periodic_clear_epoch
            .load(portable_atomic::Ordering::Relaxed);
        finalize_via_start_bracket(
            &service,
            TEST_HANDLE,
            TEST_COP_HANDLE,
            channel_id,
            j2534_0404::PeriodicMessageId(77),
            started_epoch,
        )
        .await;

        let links = service.logical_links.lock().await;
        assert_eq!(
            links
                .get(&TEST_HANDLE)
                .unwrap()
                .tp20_broadcast_periodic
                .map(|p| p.started_epoch),
            Some(N),
            "the committed entry's started_epoch must equal periodic_clear_epoch's value at the \
             moment of the native start's success"
        );
    }

    /// Sentinel deferral fix (edge-case-hunter finding, design-advisor-
    /// approved, ADR-192): starting from a sentinel a `CLEAR_PERIODIC_MSGS`
    /// scan has already stamped `pending_clear_generation` on (`rpc_misc.rs`'s
    /// `clear_periodic_msgs_epoch_gating_tests::
    /// sentinel_scan_defers_instead_of_finalizing_and_stamps_pending_clear_generation`
    /// proves the scan reaches exactly this state), a `started_epoch` at or
    /// after that stamped generation means no recorded clear's native call
    /// ran after this start's own native call returned -- the message is
    /// still live device-side, so this commits exactly like an ordinary
    /// (never-scanned) reservation: real message_id,
    /// `pending_clear_generation` reset to `0`, `dispatched`, exactly one
    /// `PduCopstExecuting`, and no native `stop_periodic_message` call (the
    /// message was never actually cleared).
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn resolves_live_when_started_epoch_at_or_after_pending_clear_generation() {
        let service = service_with_one_link(1, false).await;
        let channel_id = j2534_0404::ChannelId(42);
        const PENDING_CLEAR_GENERATION: u64 = 5;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.channel_id = Some(channel_id);
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: PENDING_CLEAR_GENERATION,
            });
        }
        service.primitives.lock().await.insert(
            TEST_COP_HANDLE,
            CopEntry {
                cll_handle: TEST_HANDLE,
                dispatched: false,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        let before = stop_periodic_call_count();
        finalize_via_start_bracket(
            &service,
            TEST_HANDLE,
            TEST_COP_HANDLE,
            channel_id,
            j2534_0404::PeriodicMessageId(77),
            PENDING_CLEAR_GENERATION,
        )
        .await;
        assert_eq!(
            stop_periodic_call_count(),
            before,
            "a live resolution must not issue a native stop_periodic_message call"
        );

        let links = service.logical_links.lock().await;
        assert_eq!(
            links.get(&TEST_HANDLE).unwrap().tp20_broadcast_periodic,
            Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: Some(j2534_0404::PeriodicMessageId(77)),
                started_epoch: PENDING_CLEAR_GENERATION,
                pending_clear_generation: 0,
            }),
            "started_epoch >= pending_clear_generation must commit live, resetting \
             pending_clear_generation to 0"
        );
        assert!(
            service
                .primitives
                .lock()
                .await
                .get(&TEST_COP_HANDLE)
                .unwrap()
                .dispatched,
            "the COP must be marked dispatched"
        );
        let queue = links.get(&TEST_HANDLE).unwrap().rx_buf.lock().await;
        assert_eq!(
            queue.items.len(),
            1,
            "exactly one status event must be queued"
        );
        assert!(
            matches!(
                queue.items.front(),
                Some(CllQueueItem::Status(TrackedStatus {
                    event: StatusEvent::Cop {
                        cop_handle: TEST_COP_HANDLE,
                        status: vci_service_interface::PduComPrimitiveStatus::PduCopstExecuting,
                        ..
                    },
                    ..
                }))
            ),
            "the queued event must be this cop_handle's own PduCopstExecuting"
        );
    }

    /// The opposite resolution: `started_epoch` strictly before the stamped
    /// `pending_clear_generation` means at least one recorded clear's
    /// native `clear_periodic_messages` call ran AFTER this start's own
    /// native call returned -- the channel-wide clear already killed this
    /// message device-side. The entry is taken via `events::
    /// emit_terminal_if_live` (never committed -- only the "live" branch
    /// above ever sets `dispatched`, so it is implicitly never set on this
    /// path either) and exactly one `PduCopstFinished` is queued, with no
    /// prior `PduCopstExecuting` -- reproducing exactly what
    /// `CLEAR_PERIODIC_MSGS`'s own scan would have emitted had it been able
    /// to resolve the sentinel at scan time. No native
    /// `stop_periodic_message` call: the clear's own native call already
    /// handled it.
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn resolves_finished_when_started_epoch_before_pending_clear_generation() {
        let service = service_with_one_link(1, false).await;
        let channel_id = j2534_0404::ChannelId(42);
        const PENDING_CLEAR_GENERATION: u64 = 5;
        const STARTED_EPOCH: u64 = 4;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.channel_id = Some(channel_id);
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: PENDING_CLEAR_GENERATION,
            });
        }
        service.primitives.lock().await.insert(
            TEST_COP_HANDLE,
            CopEntry {
                cll_handle: TEST_HANDLE,
                dispatched: false,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        let before = stop_periodic_call_count();
        finalize_via_start_bracket(
            &service,
            TEST_HANDLE,
            TEST_COP_HANDLE,
            channel_id,
            j2534_0404::PeriodicMessageId(77),
            STARTED_EPOCH,
        )
        .await;
        assert_eq!(
            stop_periodic_call_count(),
            before,
            "a Finished resolution must not issue a native stop_periodic_message call -- the \
             channel-wide clear already handled it"
        );

        let links = service.logical_links.lock().await;
        assert_eq!(
            links.get(&TEST_HANDLE).unwrap().tp20_broadcast_periodic,
            None,
            "started_epoch < pending_clear_generation must take the entry, not commit it"
        );
        assert!(
            !service
                .primitives
                .lock()
                .await
                .contains_key(&TEST_COP_HANDLE),
            "a Finished resolution removes the primitives entry via emit_terminal_if_live"
        );
        let queue = links.get(&TEST_HANDLE).unwrap().rx_buf.lock().await;
        assert_eq!(
            queue.items.len(),
            1,
            "exactly one status event must be queued"
        );
        assert!(
            matches!(
                queue.items.front(),
                Some(CllQueueItem::Status(TrackedStatus {
                    event: StatusEvent::Cop {
                        cop_handle: TEST_COP_HANDLE,
                        status: vci_service_interface::PduComPrimitiveStatus::PduCopstFinished,
                        ..
                    },
                    ..
                }))
            ),
            "the queued event must be this cop_handle's own PduCopstFinished, with no prior \
             Executing"
        );
    }

    /// The resolution half of the "double clear" scenario (the `.max()`-
    /// reachability half -- proving a SECOND racing clear advances
    /// `pending_clear_generation` to its own larger generation rather than
    /// regressing it -- is proven separately, at the scan level, by
    /// `rpc_misc.rs`'s `clear_periodic_msgs_epoch_gating_tests::
    /// second_racing_clear_does_not_regress_pending_clear_generation`, since
    /// that scan logic lives in, and is only reachable through, the
    /// `rpc_misc` module). With `pending_clear_generation` stamped at the
    /// SECOND (larger) racing clear's own generation `G2`, `started_epoch ==
    /// G1` (a value from the first, earlier clear, `G1 < G2`) still resolves
    /// Finished -- the later clear's own native call covers it too -- while
    /// a sibling case with `started_epoch == G2` resolves live.
    #[tokio::test]
    async fn double_clear_generation_resolves_finished_for_g1_and_live_for_g2() {
        const G1: u64 = 3;
        const G2: u64 = 7;

        // Sibling case 1: started_epoch == G1 (< G2) -> Finished.
        {
            let service = service_with_one_link(1, false).await;
            let channel_id = j2534_0404::ChannelId(42);
            {
                let mut links = service.logical_links.lock().await;
                let link = links.get_mut(&TEST_HANDLE).unwrap();
                link.channel_id = Some(channel_id);
                link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                    cop_handle: TEST_COP_HANDLE,
                    message_id: None,
                    started_epoch: 0,
                    pending_clear_generation: G2,
                });
            }
            service.primitives.lock().await.insert(
                TEST_COP_HANDLE,
                CopEntry {
                    cll_handle: TEST_HANDLE,
                    dispatched: false,
                    transmits: true,
                    is_send_recv: true,
                    cop_tag: None,
                },
            );

            finalize_via_start_bracket(
                &service,
                TEST_HANDLE,
                TEST_COP_HANDLE,
                channel_id,
                j2534_0404::PeriodicMessageId(77),
                G1,
            )
            .await;

            let links = service.logical_links.lock().await;
            assert_eq!(
                links.get(&TEST_HANDLE).unwrap().tp20_broadcast_periodic,
                None,
                "started_epoch == G1 (< G2) must resolve Finished even though G2 came from a \
                 SECOND racing clear, not the first"
            );
            drop(links);
            assert!(
                !service
                    .primitives
                    .lock()
                    .await
                    .contains_key(&TEST_COP_HANDLE),
                "a Finished resolution removes the primitives entry"
            );
        }

        // Sibling case 2: started_epoch == G2 (>= G2) -> live.
        {
            let service = service_with_one_link(1, false).await;
            let channel_id = j2534_0404::ChannelId(42);
            {
                let mut links = service.logical_links.lock().await;
                let link = links.get_mut(&TEST_HANDLE).unwrap();
                link.channel_id = Some(channel_id);
                link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                    cop_handle: TEST_COP_HANDLE,
                    message_id: None,
                    started_epoch: 0,
                    pending_clear_generation: G2,
                });
            }
            service.primitives.lock().await.insert(
                TEST_COP_HANDLE,
                CopEntry {
                    cll_handle: TEST_HANDLE,
                    dispatched: false,
                    transmits: true,
                    is_send_recv: true,
                    cop_tag: None,
                },
            );

            finalize_via_start_bracket(
                &service,
                TEST_HANDLE,
                TEST_COP_HANDLE,
                channel_id,
                j2534_0404::PeriodicMessageId(77),
                G2,
            )
            .await;

            let links = service.logical_links.lock().await;
            assert_eq!(
                links.get(&TEST_HANDLE).unwrap().tp20_broadcast_periodic,
                Some(Tp20BroadcastPeriodic {
                    cop_handle: TEST_COP_HANDLE,
                    message_id: Some(j2534_0404::PeriodicMessageId(77)),
                    started_epoch: G2,
                    pending_clear_generation: 0,
                }),
                "started_epoch == G2 (>= G2) must resolve live"
            );
        }
    }

    /// design-advisor-flagged trap (edge-case-hunter finding, ADR-192): if a
    /// `CLEAR_PERIODIC_MSGS` scan has already stamped a nonzero
    /// `pending_clear_generation` onto this exact `cop_handle`'s own
    /// `None`-sentinel reservation, and the native start then FAILS (the `Err`
    /// path in `rpc_start_com_primitive`, which calls
    /// `rollback_tp20_broadcast_periodic_reservation` to roll back), the
    /// rollback must still recognize and clear the entry. A literal
    /// full-struct equality check against `{ message_id: None, started_epoch:
    /// 0 }` (with no `pending_clear_generation` in the comparison) would no
    /// longer match once a clear has stamped a nonzero
    /// `pending_clear_generation` -- leaving a stale sentinel stuck forever,
    /// blocking every future start on this CLL and blocking
    /// `LOCK_PHYSICAL_TX_QUEUE` grants. This test fails before the
    /// field-wise-match fix (see
    /// `rollback_tp20_broadcast_periodic_reservation`'s own doc comment) and
    /// passes after it.
    #[tokio::test]
    async fn rollback_clears_sentinel_even_after_pending_clear_generation_stamped() {
        let service = service_with_one_link(1, false).await;
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: None,
                started_epoch: 0,
                pending_clear_generation: 1,
            });
        }
        service.primitives.lock().await.insert(
            TEST_COP_HANDLE,
            CopEntry {
                cll_handle: TEST_HANDLE,
                dispatched: false,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        service
            .rollback_tp20_broadcast_periodic_reservation(TEST_HANDLE, TEST_COP_HANDLE)
            .await;

        let links = service.logical_links.lock().await;
        let link = links.get(&TEST_HANDLE).unwrap();
        assert_eq!(
            link.tp20_broadcast_periodic, None,
            "the rollback must clear a sentinel a clear has already stamped \
             pending_clear_generation onto -- not just an untouched (pending_clear_generation \
             == 0) one"
        );
        assert!(
            link.rx_buf.lock().await.items.is_empty(),
            "a rollback must not emit any status events"
        );
    }
}

/// Codex review (PR #101, post-merge finding): `Tp20BroadcastPeriodic::
/// message_id`'s in-flight-start sentinel is `None` (out-of-band), never an
/// in-band `PeriodicMessageId(0)` value -- SAE J2534-1 clause 7.2.7.2 places
/// no floor on `PassThruStartPeriodicMsg`'s `pMsgID` output, so a
/// conformant adapter may legitimately return `0` for a genuine live
/// message. This module proves `rpc_cancel_com_primitive`'s CoptCancel
/// branch (the exact site the finding named) treats a COMMITTED entry whose
/// real adapter-assigned id happens to be `0` as a real message -- issuing
/// the native `stop_periodic_message` call -- rather than silently skipping
/// it as if it were the `None`-sentinel reservation.
#[cfg(test)]
mod rpc_cancel_com_primitive_zero_message_id_tests {
    use std::os::raw::c_long;

    use j2534_0404_sys::libloading::{Library, Symbol};
    use serial_test::serial;

    use super::rollback_stop_comm_pending_tests::{TEST_HANDLE, service_with_one_link};
    use super::*;

    /// Same technique as `finalize_or_orphan_broadcast_periodic_start_tests::
    /// stop_periodic_call_count` (see that function's own doc comment for
    /// the same-loaded-shared-object rationale this relies on).
    fn stop_periodic_call_count() -> usize {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = Library::new(&lib_path).expect("mock library should be loadable");
            let f: Symbol<unsafe extern "system" fn() -> usize> = lib
                .get(b"__mock_get_stop_periodic_count_on_current_thread\0")
                .expect("__mock_get_stop_periodic_count_on_current_thread should be exported");
            f()
        }
    }

    /// Mirrors `terminate_tp20_broadcast_periodic_for_suspension_tests::
    /// set_stop_periodic_message_error`'s own construction shape
    /// (`rpc_misc.rs`) -- used here (Codex review fix, P2, PR #101, round
    /// 14) to force the native `PassThruStopPeriodicMsg` call
    /// `rpc_cancel_com_primitive`'s `CoptCancel` branch issues to fail with
    /// an arbitrary caller-chosen code, including `ERR_INVALID_MSG_ID`.
    fn set_stop_periodic_message_error(code: Option<c_long>) {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        unsafe {
            let lib = Library::new(&lib_path).expect("mock library should be loadable");
            let f: Symbol<unsafe extern "system" fn(c_long) -> c_long> = lib
                .get(b"__mock_set_stop_periodic_message_error\0")
                .expect("__mock_set_stop_periodic_message_error should be exported");
            f(code.unwrap_or(0));
        }
    }

    const TEST_COP_HANDLE: u32 = 200;

    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn cancel_stops_a_committed_message_whose_adapter_assigned_id_is_zero() {
        let service = service_with_one_link(1, false).await;
        let channel_id = j2534_0404::ChannelId(42);
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.channel_id = Some(channel_id);
            // A COMMITTED entry (not the None-sentinel reservation) whose
            // real, adapter-assigned message_id genuinely happens to be 0 --
            // the exact case the finding named as being silently confused
            // with the in-flight-start sentinel before this fix.
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: Some(j2534_0404::PeriodicMessageId(0)),
                started_epoch: 0,
                pending_clear_generation: 0,
            });
        }
        service.primitives.lock().await.insert(
            TEST_COP_HANDLE,
            CopEntry {
                cll_handle: TEST_HANDLE,
                dispatched: true,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        let before = stop_periodic_call_count();

        service
            .rpc_cancel_com_primitive(Request::new(
                vci_service_interface::CancelComPrimitiveRequest {
                    cop_handle: Some(vci_service_interface::ComPrimitiveHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle: TEST_HANDLE,
                        cop_handle: TEST_COP_HANDLE,
                    }),
                },
            ))
            .await
            .expect("CancelComPrimitive should succeed");

        assert_eq!(
            stop_periodic_call_count(),
            before + 1,
            "Some(PeriodicMessageId(0)) is a real, committed message and must trigger a native \
             stop_periodic_message call -- if it were wrongly treated as the None-sentinel, this \
             count would not advance"
        );
        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_HANDLE)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "the tracking entry must be cleared on a successful cancel"
        );
    }

    /// Codex review fix (P2, PR #101, round 14): `ERR_INVALID_MSG_ID` from
    /// the native `PassThruStopPeriodicMsg` call this branch issues is an
    /// AUTHORITATIVE "device already forgot this message" signal (e.g. a
    /// channel-wide `CLEAR_PERIODIC_MSGS` that raced this exact
    /// `CoptCancel`, clearing the slot device-side between this branch's own
    /// `take()` above and this native stop call) -- not a genuine failure.
    /// It must fall through to the ordinary `Cancelled` success path below,
    /// not the restore-or-leak-track-then-error path a genuine failure
    /// takes (proven separately by
    /// `rpc_cancel_com_primitive_broadcast_periodic_session_gate_tests`).
    #[tokio::test]
    #[serial(tp20_stop_periodic_call_counter)]
    async fn cancel_treats_err_invalid_msg_id_from_stop_as_already_cancelled() {
        let service = service_with_one_link(1, false).await;
        let channel_id = j2534_0404::ChannelId(42);
        {
            let mut links = service.logical_links.lock().await;
            let link = links.get_mut(&TEST_HANDLE).unwrap();
            link.channel_id = Some(channel_id);
            link.tp20_broadcast_periodic = Some(Tp20BroadcastPeriodic {
                cop_handle: TEST_COP_HANDLE,
                message_id: Some(j2534_0404::PeriodicMessageId(55)),
                started_epoch: 0,
                pending_clear_generation: 0,
            });
        }
        service.primitives.lock().await.insert(
            TEST_COP_HANDLE,
            CopEntry {
                cll_handle: TEST_HANDLE,
                dispatched: true,
                transmits: true,
                is_send_recv: true,
                cop_tag: None,
            },
        );

        set_stop_periodic_message_error(Some(j2534_0404::ERR_INVALID_MSG_ID as c_long));

        let result = service
            .rpc_cancel_com_primitive(Request::new(
                vci_service_interface::CancelComPrimitiveRequest {
                    cop_handle: Some(vci_service_interface::ComPrimitiveHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle: TEST_HANDLE,
                        cop_handle: TEST_COP_HANDLE,
                    }),
                },
            ))
            .await;

        set_stop_periodic_message_error(None);

        result.expect(
            "ERR_INVALID_MSG_ID must be treated as already-cancelled, not surfaced as a gRPC \
             error",
        );

        assert!(
            !service
                .primitives
                .lock()
                .await
                .contains_key(&TEST_COP_HANDLE),
            "the COP must be removed from `primitives` -- ERR_INVALID_MSG_ID falls through to \
             the ordinary Cancelled success path, same as a real Ok(()) stop"
        );
        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_HANDLE)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "nothing must be restored onto the link -- the device says the message is already \
             gone, there is nothing to retry"
        );
        drop(links);
        assert!(
            service.shared_channels.lock().await.is_empty(),
            "nothing must be leak-tracked against a shared channel either -- there is nothing \
             left to clean up device-side"
        );
    }
}

/// Direct unit-level coverage of `rpc_cancel_com_primitive`'s `CoptCancel`
/// integration with the shared `restore_or_leak_track_broadcast_periodic`
/// helper (Codex review round 11 finding 2, PR #101): before this fix, a
/// failed native `PassThruStopPeriodicMsg` during `CoptCancel` restored the
/// entry onto the CLL unconditionally, with no session/generation/connected
/// check at all -- the exact hazard round 10 fixed for
/// `terminate_tp20_broadcast_periodic_for_suspension` (`rpc_misc.rs`) but
/// never applied here. A genuine concurrent disconnect (or
/// disconnect+reconnect) landing between `rpc_cancel_com_primitive`'s own
/// capture of `connect_generation`/`channel_key` and its failure-handling
/// re-read of the live link is infeasible to construct through the gRPC
/// layer with this crate's established test techniques -- the same class of
/// narrow-window infeasibility `terminate_tp20_broadcast_periodic_for_
/// suspension_tests` (`rpc_misc.rs`) already documents for its own gate.
/// These tests instead call the shared helper directly with the same
/// deliberately mismatched parameters that module uses, exercising the
/// identical guard logic `CoptCancel` now delegates to.
#[cfg(test)]
mod rpc_cancel_com_primitive_broadcast_periodic_session_gate_tests {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;

    const TEST_CLL: u32 = 1;
    const COP_HANDLE: u32 = 300;
    const TEST_CHANNEL_ID: ChannelId = ChannelId(9);
    const TEST_CHANNEL_KEY: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);
    /// The `connect_generation` this test's `periodic`/`channel_id` were
    /// captured against (before the simulated disconnect/reconnect).
    const CAPTURED_GENERATION: u64 = 1;
    /// The link's LIVE `connect_generation` at the time the failure branch
    /// runs -- deliberately different from `CAPTURED_GENERATION`, simulating
    /// a disconnect+reconnect landing in the gap.
    const LIVE_GENERATION: u64 = 2;

    /// Mirrors `terminate_tp20_broadcast_periodic_for_suspension_tests::
    /// live_link`'s own construction shape (`rpc_misc.rs`).
    fn live_link() -> LogicalLinkState {
        LogicalLinkState {
            channel_id: Some(TEST_CHANNEL_ID),
            protocol: ChannelProtocol::CAN,
            hw_protocol_id: j2534_0404::CAN,
            software_isotp: false,
            uudt_channel_id: None,
            uudt_channel_key: None,
            isotp_rx: Arc::new(Mutex::new(HashMap::new())),
            connect_in_flight: std::sync::Weak::new(),
            connected: true,
            comm_started: true,
            raw_mode: false,
            checksum_mode: false,
            connect_generation: LIVE_GENERATION,
            stop_comm_pending: false,
            channel_key: Some(TEST_CHANNEL_KEY),
            pin_select: None,
            channel_index: None,
            base_hw_protocol_override: None,
            rx_buf: Arc::new(Mutex::new(CllEventQueue {
                event_queue_cap: 16,
                ..CllEventQueue::default()
            })),
            working: ComParamSet::default(),
            active: ComParamSet::default(),
            tester_present_state: TesterPresentState::None,
            tester_present_base_tx_flags: 0,
            open_tp_discards: Vec::new(),
            working_unique_resp_id_table: Vec::new(),
            active_unique_resp_id_table: Vec::new(),
            unique_resp_filter_ids: Vec::new(),
            cancelled_cops: HashSet::new(),
            held_lock_mask: 0,
            last_error: None,
            tx_held: VecDeque::new(),
            tx_suspended_by_ioctl: false,
            tx_suspended_by_lock: false,
            tx_suspended_by_error: false,
            error_clear_seq: 0,
            error_set_seq: 0,
            client_filters: HashMap::new(),
            repeat_message_ids: Vec::new(),
            pending_client_filters: HashMap::new(),
            registrants: Vec::new(),
            next_registrant_seq: 0,
            j1939_claimed_address: None,
            j1939_claim_cursor: 0,
            j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
            tp20_connection: None,
            tp20_broadcast_periodic: None,
        }
    }

    fn shared_channel(ref_count: u32) -> SharedChannel {
        SharedChannel {
            channel_id: TEST_CHANNEL_ID,
            ref_count,
            tx_queue: tokio::sync::mpsc::unbounded_channel().0,
            executing_cop: Arc::new(Mutex::new(None)),
            _poll_cancel: tokio::sync::oneshot::channel().0,
            connect_flags: 0,
            dead: false,
            occupancy_epoch: 0,
            leaked_repeat_message_ids: Vec::new(),
            leaked_periodic_message_ids: Vec::new(),
            applied_analog_sample_rate: None,
            applied_analog_samples_per_reading: None,
            applied_analog_readings_per_msg: None,
            j1939_claims: HashMap::new(),
            j1939_claim_results: HashMap::new(),
            j1939_reclaim_pending: HashMap::new(),
            leaked_j1939_claims: Vec::new(),
            tp20_connections: HashMap::new(),
            tp20_connection_results: HashMap::new(),
            become_master_in_flight: Arc::new(portable_atomic::AtomicBool::new(false)),
            tp20_passive: None,
        }
    }

    /// Builds a minimal `J2534Service` with `live_link()` at `TEST_CLL` and
    /// (if `Some`) `sc` installed as the `TEST_CHANNEL_KEY` shared channel.
    /// Mirrors `terminate_tp20_broadcast_periodic_for_suspension_tests::
    /// service_with_live_link`'s own construction shape (`rpc_misc.rs`).
    async fn service_with_live_link(sc: Option<SharedChannel>) -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");

        let mut shared_channels = HashMap::new();
        if let Some(sc) = sc {
            shared_channels.insert(TEST_CHANNEL_KEY, sc);
        }

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::default(),
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(HashMap::from([(TEST_CLL, live_link())]))),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(shared_channels)),
            primitives: Arc::new(Mutex::new(HashMap::new())),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn test_periodic() -> Tp20BroadcastPeriodic {
        Tp20BroadcastPeriodic {
            cop_handle: COP_HANDLE,
            message_id: Some(j2534_0404::PeriodicMessageId(777)),
            started_epoch: 3,
            pending_clear_generation: 0,
        }
    }

    /// (a) Generation mismatch with a matching `shared_channels` entry a
    /// sibling CLL still keeps open (`ref_count > 1`): the failed stop must
    /// NOT be restored onto the (now-different-session) link, and must
    /// instead be leak-tracked against the shared channel.
    #[tokio::test]
    async fn generation_mismatch_leak_tracks_instead_of_restoring_when_channel_survives() {
        let service = service_with_live_link(Some(shared_channel(2))).await;

        service
            .restore_or_leak_track_broadcast_periodic(
                TEST_CLL,
                test_periodic(),
                j2534_0404::PeriodicMessageId(777),
                CapturedBroadcastPeriodicSession {
                    connect_generation: CAPTURED_GENERATION,
                    channel_key: Some(TEST_CHANNEL_KEY),
                    channel_id: Some(TEST_CHANNEL_ID),
                },
                "CoptCancel",
            )
            .await;

        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "a generation-mismatched failure must never restore onto the (now-different-\
             session) link"
        );
        drop(links);

        let chans = service.shared_channels.lock().await;
        let sc = chans.get(&TEST_CHANNEL_KEY).unwrap();
        assert_eq!(
            sc.leaked_periodic_message_ids,
            vec![(j2534_0404::PeriodicMessageId(777), 3)],
            "the failed stop must be leak-tracked against the surviving shared channel"
        );
    }

    /// (b) Same generation mismatch, but the physical channel has ALSO
    /// fully closed by the time this runs (no `shared_channels` entry for
    /// `channel_key`): nothing to restore, nothing to leak-track -- just the
    /// accepted-residual double-fault warn path, no panic.
    #[tokio::test]
    async fn generation_mismatch_with_no_surviving_channel_does_not_panic_or_leak() {
        let service = service_with_live_link(None).await;

        service
            .restore_or_leak_track_broadcast_periodic(
                TEST_CLL,
                test_periodic(),
                j2534_0404::PeriodicMessageId(777),
                CapturedBroadcastPeriodicSession {
                    connect_generation: CAPTURED_GENERATION,
                    channel_key: Some(TEST_CHANNEL_KEY),
                    channel_id: Some(TEST_CHANNEL_ID),
                },
                "CoptCancel",
            )
            .await;

        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "nothing must be restored when the original session is gone"
        );
        drop(links);
        assert!(
            service.shared_channels.lock().await.is_empty(),
            "there is nothing to leak-track when the physical channel has also fully closed"
        );
    }

    /// (c) Same `connect_generation`, but `connected` is `false`
    /// (simulating a plain disconnect with no reconnect -- Codex review
    /// finding 1, round 11): the failed stop must NOT be restored onto the
    /// (now-disconnected) link, and must instead be leak-tracked.
    #[tokio::test]
    async fn matching_generation_but_disconnected_leak_tracks_instead_of_restoring() {
        let service = service_with_live_link(Some(shared_channel(2))).await;
        service
            .logical_links
            .lock()
            .await
            .get_mut(&TEST_CLL)
            .unwrap()
            .connected = false;

        service
            .restore_or_leak_track_broadcast_periodic(
                TEST_CLL,
                test_periodic(),
                j2534_0404::PeriodicMessageId(777),
                CapturedBroadcastPeriodicSession {
                    connect_generation: LIVE_GENERATION,
                    channel_key: Some(TEST_CHANNEL_KEY),
                    channel_id: Some(TEST_CHANNEL_ID),
                },
                "CoptCancel",
            )
            .await;

        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .tp20_broadcast_periodic
                .is_none(),
            "a matching-generation-but-disconnected failure must never restore onto the \
             (now-disconnected) link"
        );
        drop(links);

        let chans = service.shared_channels.lock().await;
        let sc = chans.get(&TEST_CHANNEL_KEY).unwrap();
        assert_eq!(
            sc.leaked_periodic_message_ids,
            vec![(j2534_0404::PeriodicMessageId(777), 3)],
            "the failed stop must be leak-tracked against the surviving shared channel"
        );
    }
}

/// Direct unit-level coverage of ADR-115 round 6's `rpc_subscribe_event`/
/// `reconcile_stale_cll_subscription`/`rpc_create_com_logical_link` fix (PR
/// #122): two concurrent `SubscribeEvent` calls for the same `cll_handle`
/// must always leave the `subscriptions` map's stored sender for that key
/// and the CLL's own `CllEventQueue::live_sender` agreeing (via
/// `same_channel`), regardless of which call's critical section under
/// `subscriptions` runs last. Also directly exercises `terminate_subscription`
/// asserting `CllEventQueue::live_sender` identity via `same_channel` --
/// the round 4/5 generation counter this module used to exercise is gone;
/// see `docs/adr/ADR-115-pdu-evt-data-lost-emission.md`'s "Correction
/// (round 6, ...)" section for why. Renamed from
/// `rpc_subscribe_event_generation_tests` accordingly.
#[cfg(test)]
mod rpc_subscribe_event_live_sender_tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::rollback_stop_comm_pending_tests::{TEST_HANDLE, service_with_one_link};
    use super::*;

    fn subscribe_request() -> Request<vci_service_interface::SubscribeEventRequest> {
        Request::new(vci_service_interface::SubscribeEventRequest {
            handle: Some(
                vci_service_interface::subscribe_event_request::Handle::CllHandle(
                    vci_service_interface::ComLogicalLinkHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle: TEST_HANDLE,
                    },
                ),
            ),
        })
    }

    /// Two `SubscribeEvent` calls racing for the SAME, already-existing
    /// `cll_handle`: this is the exact interleaving ADR-115 round 4
    /// originally fixed with a generation counter and round 6 now fixes
    /// structurally with a co-resident `live_sender`. Forces genuine
    /// concurrent execution (rather than asserting a property that would
    /// hold trivially for two purely sequential calls) by holding
    /// `subscriptions` externally so both calls' step-1 queue-Arc
    /// pre-resolution (needs only `logical_links`, uncontended, so it
    /// proceeds immediately) completes before either call's step-2 atomic
    /// insert-and-write (needs `subscriptions`) can run.
    ///
    /// **What this test does and does not guarantee** (edge-case-hunter
    /// verification pass, round 5, still applicable after round 6's
    /// rewrite since the atomic-critical-section shape is unchanged --
    /// only what gets written into it changed): it reliably exercises two
    /// genuinely concurrent `SubscribeEvent` calls (both truly in-flight
    /// together via `tokio::spawn`, not one `.await`ed to completion before
    /// the other is even issued) and confirms the invariant under real
    /// scheduling. It does NOT reliably detect a reintroduced non-atomic
    /// split of this critical section (the historical round-4 bug shape:
    /// write the queue, THEN separately insert into `subscriptions`):
    /// manually reverted and run 5/5 times, this test still passed every
    /// time against that reverted code. Root cause: this crate's
    /// single-threaded `current_thread` test runtime never preempts a task
    /// mid-poll, only at a genuine `Pending` suspension, so once the
    /// `subscriptions` hold below is dropped, the first-queued waiter runs
    /// its ENTIRE insert-and-write sequence to completion before the other
    /// task is polled again at all -- there is no natural
    /// mid-critical-section preemption point for the old "two separate
    /// acquisitions" bug shape to land in. The `Barrier` below is a
    /// genuine, cheap improvement over relying on a fixed `yield_now` count
    /// alone to get both tasks to a simultaneous starting line, but it does
    /// not change this fundamental single-threaded scheduling property. A
    /// fully deterministic fault-injection test for this exact historical
    /// gap would need a test-only pause/gate hook inside
    /// `rpc_subscribe_event` itself (the same class of infrastructure this
    /// crate's established infeasibility notes describe elsewhere, e.g.
    /// `rollback_stop_comm_pending_tests`'s own doc comment) -- judged not
    /// worth the invasiveness for this fix; see
    /// the Prioritized
    /// Backlog for this residual.
    #[tokio::test]
    async fn concurrent_subscribe_for_existing_cll_keeps_map_and_queue_live_sender_in_agreement() {
        let service = service_with_one_link(1, false).await;
        let key: SubscriptionKey = (DEFAULT_MODULE_HANDLE, TEST_HANDLE);

        let hold = service.subscriptions.lock().await;

        // A `Barrier` forces both tasks to have ACTUALLY started -- reached
        // this exact point in their own code, not merely been spawned --
        // before either is allowed to proceed into its own
        // `rpc_subscribe_event` call, rather than relying solely on the
        // `yield_now` loop below to have given the scheduler enough turns to
        // run both up to their respective `subscriptions` lock attempts.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let svc_a = service.clone();
        let barrier_a = Arc::clone(&barrier);
        let task_a = tokio::spawn(async move {
            barrier_a.wait().await;
            svc_a.rpc_subscribe_event(subscribe_request()).await
        });
        let svc_b = service.clone();
        let barrier_b = Arc::clone(&barrier);
        let task_b = tokio::spawn(async move {
            barrier_b.wait().await;
            svc_b.rpc_subscribe_event(subscribe_request()).await
        });

        // Let both spawned tasks actually run up to (and block on) their
        // own `subscriptions` lock acquisition before releasing it -- a
        // handful of yields is enough on this crate's single-threaded
        // `current_thread` test runtime since neither task has any other
        // await point ahead of that lock acquisition (`logical_links` is
        // uncontended) besides the barrier itself, which both clear
        // together.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        drop(hold);

        let (res_a, res_b) = tokio::join!(task_a, task_b);
        let stream_a = res_a
            .expect("task should not panic")
            .expect("subscribe should succeed");
        let stream_b = res_b
            .expect("task should not panic")
            .expect("subscribe should succeed");

        let map_tx = service
            .subscriptions
            .lock()
            .await
            .get(&key)
            .expect("one of the two concurrent calls should be the current subscriber")
            .clone();
        let queue_live_sender = service
            .logical_links
            .lock()
            .await
            .get(&TEST_HANDLE)
            .expect("link should still exist")
            .rx_buf
            .lock()
            .await
            .live_sender
            .clone();

        assert!(
            queue_live_sender.is_some_and(|live| live.same_channel(&map_tx)),
            "the subscriptions map's stored sender for this key and the CLL's own queue's \
             live_sender must always be the SAME channel, regardless of which of the two \
             concurrent SubscribeEvent calls for the same cll_handle 'won'"
        );

        drop(stream_a);
        drop(stream_b);
    }

    /// Builds a minimal `J2534Service` with a pre-opened device (matching
    /// `DEFAULT_MODULE_HANDLE`) but NO logical links registered yet --
    /// unlike `service_with_one_link`, this is for tests that need
    /// `rpc_create_com_logical_link` to actually run and allocate a handle,
    /// not just a pre-existing link to subscribe against. Built by clearing
    /// `service_with_one_link`'s single pre-populated link rather than
    /// duplicating its full `J2534Service` literal: `next_cll_handle` is
    /// still untouched (`0`) at that point, so `logical_links` being empty
    /// makes `rpc_create_com_logical_link`'s own `next_logical_link_handle`
    /// deterministically assign `TEST_HANDLE` (`1`) to the FIRST CLL it
    /// creates -- letting a test pre-compute, before `CreateComLogicalLink`
    /// ever runs, exactly the handle a concurrent `SubscribeEvent` call
    /// should target. The device is pre-opened directly via `self.api`
    /// (mirroring
    /// `rpc_link::tests::probe_can_channel_mode_does_not_cache_for_a_dead_handle_with_a_device_open`)
    /// so `ensure_open_device_for`'s "already open, return it" fast path is
    /// taken with no further `.await` inside it -- keeping `logical_links`
    /// the only lock either call path genuinely contends on here.
    async fn service_with_no_links_and_open_device() -> J2534Service {
        let service = service_with_one_link(1, false).await;
        service.logical_links.lock().await.clear();
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service
    }

    /// A minimal, valid `CreateComLogicalLink` request for `DEFAULT_MODULE_HANDLE`
    /// -- a raw CAN protocol id, no bustype (defaults are irrelevant to this
    /// module's live-sender-agreement assertions).
    fn create_request() -> Request<vci_service_interface::CreateComLogicalLinkRequest> {
        Request::new(vci_service_interface::CreateComLogicalLinkRequest {
            module_handle: Some(vci_service_interface::ModuleHandle {
                module_handle: DEFAULT_MODULE_HANDLE,
            }),
            resource: Some(
                vci_service_interface::create_com_logical_link_request::Resource::RscData(
                    vci_service_interface::ResourceData {
                        dlc_pin_data: vec![],
                        bus_type: None,
                        protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                            j2534_0404::CAN,
                        )),
                    },
                ),
            ),
            cll_create_flag: None,
        })
    }

    /// Finding 1 (edge-case-hunter verification pass, round 5, still
    /// applicable after round 6's rewrite): the only existing coverage of
    /// `rpc_create_com_logical_link`'s atomic seed-read-then-insert critical
    /// section
    /// (`subscribe_before_create_reconciles_and_delivers_live_after_creation`,
    /// `pdu_ioctl.rs`) is purely sequential -- `SubscribeEvent` fully
    /// completes before `CreateComLogicalLink` is even called -- so it
    /// verifies the seed correctly copies an already-installed
    /// subscription's sender, but never actually exercises the
    /// seed-read-and-insert being atomic against a genuinely RACING
    /// `SubscribeEvent` call. This races a `SubscribeEvent` call for
    /// `TEST_HANDLE` against a `CreateComLogicalLink` call that goes on to
    /// allocate exactly that handle (`service_with_no_links_and_open_device`'s
    /// own doc comment explains why this is deterministic, not lucky),
    /// forcing genuine overlapping execution via `tokio::spawn` for both:
    /// holds `logical_links` externally -- the first lock BOTH paths
    /// contend on (`SubscribeEvent`'s own `queue_before` pre-read locks only
    /// `logical_links`; `CreateComLogicalLink`'s `next_logical_link_handle`
    /// and its own atomic seed-read+insert critical section both need it
    /// too) -- so both tasks are genuinely blocked and in-flight together
    /// before either can proceed, then releases it and lets both race to
    /// completion. Traced by hand for both possible orderings of the nested
    /// `subscriptions` critical sections (whichever call's own atomic
    /// section reaches `subscriptions` first still observes -- or is
    /// observed by -- the other's write via the existing reconciliation
    /// steps on both sides), the map's stored sender for this key and the
    /// newly-created CLL's own `live_sender` must always be the same
    /// channel, regardless of which RPC call's spawn happens to win the
    /// actual work.
    ///
    /// **What this test does and does not guarantee** (empirically checked,
    /// not just reasoned about): it reliably exercises `SubscribeEvent` and
    /// `CreateComLogicalLink` genuinely in-flight together for the same
    /// not-yet-existing handle -- both truly concurrent via `tokio::spawn`,
    /// forced to block on the SAME `logical_links` acquisition before
    /// either can proceed -- closing the "no test exercises this
    /// interleaving at all" gap this test exists for. It does NOT reliably
    /// detect a reintroduced non-atomic split of
    /// `rpc_create_com_logical_link`'s critical section (reverting the
    /// seed-read and the `logical_links` insert back to two separate lock
    /// acquisitions, the exact historical bug shape): manually reverted and
    /// run 5/5 times, this test still passed every time against that
    /// reverted code. Root cause traced: this crate's single-threaded
    /// `current_thread` test runtime never preempts a task mid-poll, only
    /// at a genuine `Pending` suspension -- once the `logical_links` hold
    /// below is dropped, the first-queued waiter (deterministically the
    /// `SubscribeEvent` task, given this test's spawn order) runs its
    /// entire remaining sequence to completion, INCLUDING its own
    /// `subscriptions` insert, before the `CreateComLogicalLink` task is
    /// polled again at all -- so by the time the reverted code's separate
    /// `subscriptions`-only seed-read runs, `SubscribeEvent`'s insert is
    /// already durable and uncontended, and the two still happen to agree
    /// even without atomicity. Kept anyway: it still closes the literal gap
    /// this test was added for (concurrent, not sequential, coverage of
    /// this interleaving), and the assertion is still meaningful for the
    /// CURRENT, correct implementation. See the Prioritized Backlog note
    /// this residual shares with the sibling test
    /// above.
    #[tokio::test]
    async fn concurrent_subscribe_and_create_for_the_same_not_yet_existing_cll_handle_keeps_map_and_queue_live_sender_in_agreement()
     {
        let service = service_with_no_links_and_open_device().await;
        let key: SubscriptionKey = (DEFAULT_MODULE_HANDLE, TEST_HANDLE);

        let hold = service.logical_links.lock().await;

        let barrier = Arc::new(tokio::sync::Barrier::new(2));

        let svc_sub = service.clone();
        let barrier_sub = Arc::clone(&barrier);
        let sub_task = tokio::spawn(async move {
            barrier_sub.wait().await;
            svc_sub.rpc_subscribe_event(subscribe_request()).await
        });
        let svc_create = service.clone();
        let barrier_create = Arc::clone(&barrier);
        let create_task = tokio::spawn(async move {
            barrier_create.wait().await;
            svc_create
                .rpc_create_com_logical_link(create_request())
                .await
        });

        // Let both spawned tasks actually run up to (and block on) their
        // own `logical_links` lock acquisition before releasing it -- same
        // technique as the sibling test above; neither task has any other
        // genuine suspension point ahead of that lock acquisition
        // (`device_id` inside `ensure_open_device_for` is uncontended, and
        // pre-opened here so its fast path takes no further `.await`).
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        drop(hold);

        let (sub_res, create_res) = tokio::join!(sub_task, create_task);
        let stream = sub_res
            .expect("subscribe task should not panic")
            .expect("subscribe should succeed");
        let cll_response = create_res
            .expect("create task should not panic")
            .expect("create should succeed")
            .into_inner();
        let created_handle = cll_response
            .cll_handle
            .expect("cll_handle should be present")
            .cll_handle;
        assert_eq!(
            created_handle, TEST_HANDLE,
            "sanity check: this test's whole premise is that CreateComLogicalLink assigns \
             exactly the handle SubscribeEvent already targeted"
        );

        let map_tx = service
            .subscriptions
            .lock()
            .await
            .get(&key)
            .expect("the subscription should still be recorded")
            .clone();
        let queue_live_sender = service
            .logical_links
            .lock()
            .await
            .get(&TEST_HANDLE)
            .expect("the newly-created link should exist")
            .rx_buf
            .lock()
            .await
            .live_sender
            .clone();

        assert!(
            queue_live_sender.is_some_and(|live| live.same_channel(&map_tx)),
            "the subscriptions map's stored sender for this key and the newly-created CLL's \
             own live_sender must always be the same channel, regardless of whether \
             SubscribeEvent's or CreateComLogicalLink's atomic critical section observed the \
             other's write first"
        );

        drop(stream);
    }

    /// The straightforward, non-racing "create-before-subscribe" ordering
    /// (the CLL already exists when `SubscribeEvent` is called): confirms
    /// the atomic insert-and-write critical section (step 2) leaves the map
    /// and the queue agreeing even with no concurrency involved at all --
    /// the baseline the concurrent test above is a generalization of.
    #[tokio::test]
    async fn subscribe_for_already_existing_cll_writes_queue_live_sender_matching_the_map() {
        let service = service_with_one_link(1, false).await;
        let key: SubscriptionKey = (DEFAULT_MODULE_HANDLE, TEST_HANDLE);

        let response = service
            .rpc_subscribe_event(subscribe_request())
            .await
            .expect("subscribe should succeed");

        let map_tx = service
            .subscriptions
            .lock()
            .await
            .get(&key)
            .expect("subscription should be recorded")
            .clone();
        let queue_live_sender = service
            .logical_links
            .lock()
            .await
            .get(&TEST_HANDLE)
            .expect("link should still exist")
            .rx_buf
            .lock()
            .await
            .live_sender
            .clone();

        assert!(
            queue_live_sender.is_some_and(|live| live.same_channel(&map_tx)),
            "queue live_sender must be the same channel as the map's stored sender"
        );

        drop(response);
    }

    /// `reconcile_stale_cll_subscription` is a no-op when the `tx` it was
    /// called with has already been displaced from `subscriptions` by a
    /// later `SubscribeEvent` call for the same key -- exercising a
    /// delayed/stale reconciliation directly, since reliably forcing this
    /// exact interleaving through `rpc_subscribe_event` itself (subscribe A,
    /// get displaced by subscribe B, THEN A's own delayed reconciliation
    /// step finally runs) is not practical through this crate's
    /// `current_thread` test runtime (same infeasibility class as
    /// `rollback_stop_comm_pending_tests`).
    #[tokio::test]
    async fn reconcile_stale_cll_subscription_is_a_noop_when_displaced() {
        let service = service_with_one_link(1, false).await;
        let key: SubscriptionKey = (DEFAULT_MODULE_HANDLE, TEST_HANDLE);
        let queue = Arc::new(Mutex::new(CllEventQueue::default()));

        // Simulate: this call's own SubscribeEvent's `tx_a`, but was itself
        // displaced by a LATER SubscribeEvent call (`tx_b`) for the same key
        // before its own reconciliation ran -- the map's current entry now
        // carries `tx_b`, not `tx_a`. The queue's `live_sender` here (`None`,
        // unchanged from the default) stands in for whatever `tx_b`'s own
        // write (or `rpc_create_com_logical_link`'s seed-read) already
        // correctly wrote -- what matters for this test is only that
        // `tx_a`'s reconciliation must not touch it.
        let (tx_a, _rx_a) = mpsc::unbounded_channel();
        let (tx_b, _rx_b) = mpsc::unbounded_channel();
        service.subscriptions.lock().await.insert(key, tx_b);

        service
            .reconcile_stale_cll_subscription(key, &tx_a, &queue)
            .await;

        assert!(
            queue.lock().await.live_sender.is_none(),
            "a displaced subscriber's delayed reconciliation must not write the queue"
        );
    }

    /// The positive counterpart: `reconcile_stale_cll_subscription` DOES
    /// write the queue when the `tx` it was called with is STILL the
    /// current entry for that key -- i.e. no later `SubscribeEvent` call has
    /// displaced it.
    #[tokio::test]
    async fn reconcile_stale_cll_subscription_writes_when_still_current() {
        let service = service_with_one_link(1, false).await;
        let key: SubscriptionKey = (DEFAULT_MODULE_HANDLE, TEST_HANDLE);
        let queue = Arc::new(Mutex::new(CllEventQueue::default()));

        let (tx, _rx) = mpsc::unbounded_channel();
        service.subscriptions.lock().await.insert(key, tx.clone());

        service
            .reconcile_stale_cll_subscription(key, &tx, &queue)
            .await;

        assert!(
            queue
                .lock()
                .await
                .live_sender
                .as_ref()
                .is_some_and(|live| live.same_channel(&tx)),
            "a still-current reconciliation must write the queue with its own sender"
        );
    }

    /// Edge-case-hunter item (b), ADR-115 round 6: after
    /// `terminate_subscription` removes a CLL-keyed subscription, a
    /// subsequent push (`send_error_event`, chosen here since it is the one
    /// `deliver_or_enqueue` call site reachable via a `pub(super)` function
    /// from this test module -- see that function's own doc comment) is NOT
    /// delivered to the now-orphaned channel and instead lands in the
    /// queue. Directly asserts `live_sender` becomes `None` (the mechanism
    /// this depends on), not just the item's own survival in the queue.
    #[tokio::test]
    async fn terminate_subscription_clears_live_sender_and_stops_future_delivery() {
        let service = service_with_one_link(1, false).await;
        let key: SubscriptionKey = (DEFAULT_MODULE_HANDLE, TEST_HANDLE);

        let stream = service
            .rpc_subscribe_event(subscribe_request())
            .await
            .expect("subscribe should succeed");

        let queue = service
            .logical_links
            .lock()
            .await
            .get(&TEST_HANDLE)
            .expect("link should exist")
            .rx_buf
            .clone();
        assert!(
            queue.lock().await.live_sender.is_some(),
            "sanity check: live_sender should be populated immediately after subscribing"
        );

        service.terminate_subscription(key, Some(&queue)).await;

        assert!(
            queue.lock().await.live_sender.is_none(),
            "terminate_subscription must clear live_sender for the CLL it was backing"
        );
        assert!(
            !service.subscriptions.lock().await.contains_key(&key),
            "terminate_subscription must remove the subscriptions map entry too"
        );

        // A push after termination must land in the queue, not be delivered
        // live to the now-orphaned channel -- guaranteed structurally by
        // live_sender being None, which deliver_or_enqueue always reads
        // fresh under the queue lock (ADR-115 round 6).
        super::events::send_error_event(
            &service.subscriptions,
            &service.logical_links,
            TEST_HANDLE,
            vci_service_interface::PduErrorEvent::PduErrEvtLostCommToVci,
            None,
        )
        .await;

        assert_eq!(
            queue.lock().await.items.len(),
            1,
            "the push after termination must be enqueued, not delivered to the orphaned channel"
        );

        drop(stream);
    }

    /// Closes the coverage gap the test above doesn't: this drives
    /// `live_sender` clearing through the REAL production call path
    /// (`rpc_destroy_com_logical_link`) instead of calling
    /// `terminate_subscription` directly with `logical_links` still
    /// populated -- a context that never occurs in production, since the
    /// sole real call site removes the CLL from `logical_links` *before*
    /// calling `terminate_subscription`. An edge-case-hunter pass on round
    /// 6's first cut found `terminate_subscription`'s own `live_sender`
    /// clear was dead code at exactly this call site for exactly this
    /// reason (a self-lookup against an already-emptied `logical_links`
    /// always found nothing) -- fixed by having `rpc_destroy_com_logical_link`
    /// pass its own already-in-scope `link.rx_buf` through directly instead
    /// of letting `terminate_subscription` re-derive it. This test would
    /// fail against that first cut and passes against the fix.
    #[tokio::test]
    async fn destroy_com_logical_link_clears_live_sender_via_the_real_call_path() {
        let service = service_with_one_link(1, false).await;

        let stream = service
            .rpc_subscribe_event(subscribe_request())
            .await
            .expect("subscribe should succeed");

        // Clone the queue's Arc BEFORE destroy removes it from
        // `logical_links` -- the Arc keeps the queue alive so its
        // `live_sender` can still be inspected afterward, exactly as a
        // straggling poll-task's own pre-teardown clone would.
        let queue = service
            .logical_links
            .lock()
            .await
            .get(&TEST_HANDLE)
            .expect("link should exist")
            .rx_buf
            .clone();
        assert!(
            queue.lock().await.live_sender.is_some(),
            "sanity check: live_sender should be populated immediately after subscribing"
        );

        service
            .rpc_destroy_com_logical_link(Request::new(
                vci_service_interface::DestroyComLogicalLinkRequest {
                    cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle: TEST_HANDLE,
                    }),
                },
            ))
            .await
            .expect("destroy should succeed");

        assert!(
            queue.lock().await.live_sender.is_none(),
            "DestroyComLogicalLink must clear the destroyed CLL's live_sender via the real \
             call path, not just when terminate_subscription is called directly"
        );
        assert!(
            !service
                .logical_links
                .lock()
                .await
                .contains_key(&TEST_HANDLE),
            "sanity check: the CLL should actually be gone from logical_links"
        );

        drop(stream);
    }
}

/// PR #78 edge-case-hunter follow-up: `CoptStopcomm`'s cancel-all block
/// gained the same `events::drain_cancelled_cop_if_finalized` batch drain
/// loop as `PDU_IOCTL_CLEAR_TX_QUEUE`'s handler (`rpc_misc.rs`), right after
/// its own `cancelled_cops.extend`. Unlike site 2, this block does not have
/// a tier-2-exclusion correctness bug to cover -- ADR-100 S5's own tier-2
/// exclusion is deliberately NOT applied here (a `CoptStopcomm` marks every
/// queued primitive, tier-2 registrants included; `reap_cancelled_detached_
/// registrants` is what actually consumes those marks), so the only new
/// behavior to verify at this site is the drain loop's steady-state no-op
/// on a still-live cop. The genuine reap race the loop guards against is
/// covered at the loop-shape level by `events_drain_cancelled_cop_if_
/// finalized_tests.rs`'s `batch_drain_loop_drains_only_the_stranded_marks_
/// in_a_mixed_batch` -- see that test's own doc comment for why reproducing
/// it end-to-end through `rpc_start_com_primitive` itself is infeasible
/// without a test-only pause hook (the same class of infeasibility this
/// file's own `rollback_stop_comm_pending_tests` module doc comment
/// documents for a different narrow race).
#[cfg(test)]
mod copt_stopcomm_cancel_all_cancelled_cops_tests {
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::Arc;

    use tokio::sync::{Mutex, mpsc};

    use super::*;

    const TEST_CLL: u32 = 1;
    const OTHER_QUEUED_COP: u32 = 901;
    const CONNECT_GENERATION: u64 = 3;

    /// A connected, `comm_started: true` `LogicalLinkState` sharing
    /// `channel_key`/`channel_id` with a `SharedChannel` this helper also
    /// inserts -- mirrors `rpc_misc.rs::repeat_message_leak_tests::
    /// service_with_two_sibling_clls_sharing_a_channel`'s construction
    /// shape, reduced to a single CLL.
    async fn service_with_a_started_link() -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
        let device_id = api.open(None).expect("mock open");
        let channel_id = api
            .connect(device_id, j2534_0404::CAN, 0, 500_000)
            .expect("mock connect");
        let channel_key: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);

        let mut logical_links = HashMap::new();
        logical_links.insert(
            TEST_CLL,
            LogicalLinkState {
                channel_id: Some(channel_id),
                protocol: ChannelProtocol::CAN,
                hw_protocol_id: j2534_0404::CAN,
                software_isotp: false,
                uudt_channel_id: None,
                uudt_channel_key: None,
                isotp_rx: Arc::new(Mutex::new(HashMap::new())),
                connect_in_flight: std::sync::Weak::new(),
                connected: true,
                comm_started: true,
                raw_mode: false,
                checksum_mode: false,
                connect_generation: CONNECT_GENERATION,
                stop_comm_pending: false,
                channel_key: Some(channel_key),
                pin_select: None,
                channel_index: None,
                base_hw_protocol_override: None,
                rx_buf: Arc::new(Mutex::new(CllEventQueue {
                    event_queue_cap: 16,
                    ..CllEventQueue::default()
                })),
                working: ComParamSet::default(),
                active: ComParamSet::default(),
                tester_present_state: TesterPresentState::None,
                tester_present_base_tx_flags: 0,
                open_tp_discards: Vec::new(),
                working_unique_resp_id_table: Vec::new(),
                active_unique_resp_id_table: Vec::new(),
                unique_resp_filter_ids: Vec::new(),
                cancelled_cops: HashSet::new(),
                held_lock_mask: 0,
                last_error: None,
                tx_held: VecDeque::new(),
                tx_suspended_by_ioctl: false,
                tx_suspended_by_lock: false,
                tx_suspended_by_error: false,
                error_clear_seq: 0,
                error_set_seq: 0,
                client_filters: HashMap::new(),
                repeat_message_ids: Vec::new(),
                pending_client_filters: HashMap::new(),
                registrants: Vec::new(),
                next_registrant_seq: 0,
                j1939_claimed_address: None,
                j1939_claim_cursor: 0,
                j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
                tp20_connection: None,
                tp20_broadcast_periodic: None,
            },
        );

        let mut primitives = HashMap::new();
        primitives.insert(
            OTHER_QUEUED_COP,
            CopEntry {
                cll_handle: TEST_CLL,
                dispatched: false,
                transmits: false,
                is_send_recv: false,
                cop_tag: None,
            },
        );

        let mut shared_channels = HashMap::new();
        // The receiver is intentionally dropped: this test module only
        // exercises the `cancelled_cops`/drain-loop bookkeeping that runs
        // BEFORE this RPC's own `tx_queue.send(TxItem::StopComm { .. })` --
        // a closed-channel send failure afterward (surfaced as an
        // `Err(Status::internal(..))` return, handled by this same RPC's
        // existing rollback path) does not undo that earlier bookkeeping.
        let (tx_tx, _tx_rx) = mpsc::unbounded_channel();
        let (cancel_tx, _cancel_rx) = tokio::sync::oneshot::channel();
        shared_channels.insert(
            channel_key,
            SharedChannel {
                channel_id,
                ref_count: 1,
                tx_queue: tx_tx,
                executing_cop: Arc::new(Mutex::new(None)),
                _poll_cancel: cancel_tx,
                connect_flags: 0,
                dead: false,
                occupancy_epoch: 0,
                leaked_repeat_message_ids: Vec::new(),
                leaked_periodic_message_ids: Vec::new(),
                applied_analog_sample_rate: None,
                applied_analog_samples_per_reading: None,
                applied_analog_readings_per_msg: None,
                j1939_claims: HashMap::new(),
                j1939_claim_results: HashMap::new(),
                j1939_reclaim_pending: HashMap::new(),
                leaked_j1939_claims: Vec::new(),
                tp20_connections: HashMap::new(),
                tp20_connection_results: HashMap::new(),
                become_master_in_flight: Arc::new(portable_atomic::AtomicBool::new(false)),
                tp20_passive: None,
            },
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        std::mem::forget(shutdown_tx);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::default(),
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_id: Arc::new(Mutex::new(Some((DEFAULT_MODULE_HANDLE, device_id)))),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(logical_links)),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(shared_channels)),
            primitives: Arc::new(Mutex::new(primitives)),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(0)),
            next_cop_handle: Arc::new(Mutex::new(0)),
            next_connect_generation: Arc::new(Mutex::new(0)),
            next_occupancy_epoch: Arc::new(Mutex::new(0)),
            subscriptions: Arc::new(Mutex::new(HashMap::new())),
            shutdown: shutdown_rx,
            module_state: Arc::new(Mutex::new(ModuleState::default())),
            module_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            system_event_buf: Arc::new(Mutex::new(VecDeque::new())),
            j1850_bus_flavor: Arc::new(Mutex::new(None)),
            prog_voltage: Arc::new(Mutex::new(HashMap::new())),
            discovery_device_info: Arc::new(Mutex::new(HashMap::new())),
            discovery_protocol_info: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// A `CoptStopcomm` request with empty `cop_data`: no native transmit,
    /// no receive resolution -- reaches the cancel-all block with only
    /// `self.primitives`/`self.logical_links` state, exactly like every
    /// other request this module builds.
    fn stop_comm_request() -> Request<vci_service_interface::StartComPrimitiveRequest> {
        Request::new(vci_service_interface::StartComPrimitiveRequest {
            cop_tag: None,
            cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                module_handle: DEFAULT_MODULE_HANDLE,
                cll_handle: TEST_CLL,
            }),
            cop_type: vci_service_interface::ComOperationType::CoptStopcomm as i32,
            cop_data: vec![],
            cop_ctrl_data: None,
        })
    }

    /// (b) A queued cop that stays live in `primitives` throughout the
    /// call: the cancel-all block's extend marks it, and the new drain
    /// loop's per-cop `drain_cancelled_cop_if_finalized` check sees it
    /// still present in `primitives` and leaves the mark alone.
    #[tokio::test]
    async fn leaves_a_still_live_queued_cops_mark_in_place() {
        let service = service_with_a_started_link().await;

        // Ignore the RPC's own return value: the `tx_queue.send` after this
        // module's own target block will fail (receiver dropped above,
        // deliberately) and roll back only `STOPPING_COP`'s own
        // `primitives` entry via the existing `rollback_stop_comm_pending`
        // path -- irrelevant to this test's assertions about
        // `OTHER_QUEUED_COP`.
        let _ = service.rpc_start_com_primitive(stop_comm_request()).await;

        assert!(
            service
                .primitives
                .lock()
                .await
                .contains_key(&OTHER_QUEUED_COP),
            "CoptStopcomm's cancel-all block must not remove a queued cop from primitives"
        );
        let links = service.logical_links.lock().await;
        assert!(
            links
                .get(&TEST_CLL)
                .unwrap()
                .cancelled_cops
                .contains(&OTHER_QUEUED_COP),
            "a still-live queued cop must be marked cancelled and its mark left in place"
        );
    }
}
