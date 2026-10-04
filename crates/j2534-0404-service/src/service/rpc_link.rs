use std::ops::RangeInclusive;
use std::sync::{Arc, atomic::Ordering};

use tonic::Code;
use tracing::{debug, info, warn};
use vci_service_interface::PduError;

use super::{
    PARAM_5BAUD_ADDR_FUNC, PARAM_5BAUD_ADDR_PHYS, PARAM_FUNC_REQ_FORMAT_PRIORITY,
    PARAM_FUNC_REQ_TARGET_ADDR, PARAM_INIT_SETTINGS, PARAM_PHYS_REQ_FORMAT_PRIORITY,
    PARAM_PHYS_REQ_TARGET_ADDR, comparam_defaults, comparam_support, discovery, events, *,
};
use crate::error::{
    gm_uart_shared_channel_locked_status, map_native_error_as, map_native_error_for_link,
    state_guard_status, unknown_handle_status,
};

/// SAE J1850 OBD-II functional-request frame used by the VPW/PWM auto-detect
/// probe (ADR-070, `Self::probe_sae_j1850_flavor`) for the OBD-active
/// resource rows (`ISO_15031_5_ON_SAE_J1850`, 0x021C/0x021D): format/priority
/// byte (0x68 VPW), target 0x6A (the standard SAE J1850 OBD functional
/// target address -- the same default `tx_header::j1850_header_bytes` falls
/// back to under functional addressing), source 0xF1 (this service's default
/// tester address), then Mode 01 (show current data) PID 00 (PIDs
/// supported), the one request every compliant OBD-II ECU answers.
const J1850_OBD_PROBE_VPW: [u8; 5] = [0x68, 0x6A, 0xF1, 0x01, 0x00];
/// PWM counterpart of [`J1850_OBD_PROBE_VPW`]: identical target/source/
/// payload, format/priority byte 0x61 (PWM).
const J1850_OBD_PROBE_PWM: [u8; 5] = [0x61, 0x6A, 0xF1, 0x01, 0x00];

/// A resolved `GetResourceStatus`/`GetConflictingResources` match candidate:
/// a `ChannelProtocol` plus the `hw_protocol_override` (when the matched row
/// has one) that must also equal a link's own `hw_protocol_id` for the match
/// to count -- see `rpc_get_resource_status` and `rpc_get_conflicting_resources`.
type ResolvedProtocolCandidate = (ChannelProtocol, Option<u32>);

/// SAE J2534-2 clause 10 Analog Inputs connect-time sync data (ADR-216
/// Decision items 6/10, amended by Codex review, PR #130, Finding 1):
/// `(capability, samples_per_reading, readings_per_msg)`, threaded from
/// `rpc_connect_com_logical_link` into `finalize_connected_link`'s own
/// connected-publishing critical section -- see that function's doc comment
/// for the full shape and the race this closes.
type AnalogConnectSync = (Option<[(ComParamId, u32); 5]>, Option<u32>, Option<u32>);

/// Outcome of [`J2534Service::probe_sae_j1850_flavor`] (ADR-070
/// verification-pass fix): whether the resolved flavor is authoritative
/// enough to cache module-wide, or only good enough to unblock the current
/// CLL.
///
/// A passive listen (the J2190 resource, which has no universal probe
/// request to transmit) seeing no traffic, or any candidate's hardware
/// probe timing out/erroring, is genuinely inconclusive -- caching it as
/// `VPW` would permanently deny a later CLL that *can* actively probe (the
/// OBD-capable resource, `ISO_15031_5_ON_SAE_J1850`) the chance to run a
/// real probe on the same module. An actual response observed on a
/// candidate (whether elicited by an active probe write or merely
/// overheard passively) is conclusive either way: real bus traffic decoded
/// at a given baud rate is real evidence, active probe or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum J1850ProbeOutcome {
    /// A response was actually observed on one of the two candidates --
    /// authoritative for the whole module, so the caller caches it.
    Conclusive(u32),
    /// Neither candidate produced a response (or the probe device could not
    /// even be opened). The wrapped value is still applied to the current
    /// link (the pre-ADR-070 VPW default), but the caller must NOT cache it.
    Fallback(u32),
}

impl J1850ProbeOutcome {
    /// The resolved native J2534 protocol ID, regardless of conclusiveness --
    /// always what the current CLL should connect with.
    fn flavor(self) -> u32 {
        match self {
            Self::Conclusive(flavor) | Self::Fallback(flavor) => flavor,
        }
    }
}

/// Installs a pass-all `PASS_FILTER` on a non-ISO15765 `channel_id` so the
/// adapter delivers all received frames to the service layer.
///
/// J2534 adapters discard all received frames unless at least one filter is
/// installed.  Service-level filtering (UniqueRespIdTable / expected-response
/// matching) is applied by the poll task, so non-ISO15765 hardware filters are
/// kept wide-open.
///
/// Per the J2534 v04.04 spec, `PASS_FILTER`/`BLOCK_FILTER` are only valid on
/// non-ISO15765 channels, and `FLOW_CONTROL_FILTER` is only valid on ISO15765
/// channels — the two families are mutually exclusive per protocol (ADR-038).
/// ISO15765 channels have no pass-all equivalent here: `ConnectComLogicalLink`
/// builds point-to-point `FLOW_CONTROL_FILTER`s directly from the CLL's
/// UniqueRespIdTable instead, with no wide-open fallback (ADR-048). Must not
/// be called with `protocol_id == ISO15765`.
///
/// `connect_flags` is the value the channel was connected with (see
/// `connect_flags` below). A TxFlags-0 `PASS_FILTER` only matches 11-bit CAN
/// Ids, so for `protocol_id == CAN` a channel connected with `CAN_ID_BOTH`
/// gets two filters — one per ID type — and one connected with plain
/// `CAN_29BIT_ID` gets a single 29-bit filter; every other protocol is
/// unaffected by `connect_flags` (ADR-065).
pub(super) fn install_pass_all_filter(
    api: &J2534Api0404,
    channel_id: ChannelId,
    protocol_id: u32,
    connect_flags: u32,
) -> Result<(), j2534_0404::Error> {
    debug_assert_ne!(protocol_id, j2534_0404::ISO15765);
    // ADR-157 Plane B: `can_filter_tx_flags`'s CAN-family check must see the
    // base protocol id even when `protocol_id` itself is a `_PS` variant
    // (the message-building calls below still use the raw `protocol_id`,
    // Plane A -- this normalization is scoped to this one call).
    for &flags in can_filter_tx_flags(resources::base_protocol_id(protocol_id), connect_flags) {
        let mut mask = j2534_0404::PassThruMessage::new(protocol_id, 0, flags, 0, 0, &[0, 0, 0, 0])
            .expect("4-byte zero mask is always valid");
        let mut pattern =
            j2534_0404::PassThruMessage::new(protocol_id, 0, flags, 0, 0, &[0, 0, 0, 0])
                .expect("4-byte zero pattern is always valid");
        api.start_message_filter(
            channel_id,
            j2534_0404::PASS_FILTER,
            &mut mask,
            &mut pattern,
            None,
        )?;
    }
    Ok(())
}

/// The `PASSTHRU_MSG.TxFlags` variants a hardware filter must be installed
/// under to match every CAN ID width `connect_flags` accepts on this channel
/// (ADR-065): a `TxFlags = 0` filter only matches 11-bit CAN Ids, so
/// `CAN_ID_BOTH` needs one filter per width and plain `CAN_29BIT_ID` needs
/// the 29-bit variant instead of the default. Every other protocol, and CAN
/// without either flag, needs exactly one filter at `TxFlags = 0`. Shared by
/// [`install_pass_all_filter`] and [`install_client_message_filters`] so the
/// two variant-selection rules can never drift apart.
///
/// Round-18 fix (Codex review, PR #72): the 29-bit branch originally
/// special-cased `protocol_id == CAN` only, so a J1939 channel -- whose
/// `connect_flags` unconditionally carries `CAN_29BIT_ID` (that function's
/// own `PROTOCOL_J1939_PS` arm) -- fell through to the `else` branch and
/// still installed its filters at `TxFlags = 0` (11-bit only), leaving the
/// channel deaf to its own 29-bit traffic despite connecting in the correct
/// mode. Widened to also recognize `PROTOCOL_J1939_PS`; J1939 never reaches
/// the `CAN_ID_BOTH` branch above it (its own `connect_flags` arm never sets
/// that bit), so it only ever needs the single `TX_EXTENDED_ID` variant,
/// never the two-filter `CAN_ID_BOTH` shape.
fn can_filter_tx_flags(protocol_id: u32, connect_flags: u32) -> &'static [u32] {
    if protocol_id == j2534_0404::CAN && connect_flags & j2534_0404::CAN_ID_BOTH != 0 {
        &[0, j2534_0404::TX_EXTENDED_ID]
    } else if (protocol_id == j2534_0404::CAN || protocol_id == j2534_0404::PROTOCOL_J1939_PS)
        && connect_flags & j2534_0404::CAN_29BIT_ID != 0
    {
        &[j2534_0404::TX_EXTENDED_ID]
    } else {
        &[0]
    }
}

/// A native-call or request-shape failure from [`install_client_message_filters`].
/// `Status` carries a request-shape problem (unrecognized/unspecified
/// `filter_type`, malformed mask/pattern) -- unreachable in practice when the
/// caller has already validated `filters` (both call sites do), kept only so
/// the closure inside `install_client_message_filters` can use `?` uniformly.
/// `Native` is a real `PassThruStartMsgFilter` failure and is the only variant
/// either call site needs to map to a client-facing error.
pub(super) enum InstallFilterFailure {
    Status(Status),
    Native(j2534_0404::Error),
}

/// Installs `filters` onto `channel_id`, one native `PassThruStartMsgFilter`
/// call per applicable `TxFlags`/ID-width variant (`can_filter_tx_flags`,
/// ADR-065). If any single native call fails, every filter this call itself
/// already installed is rolled back (best-effort; an individual rollback
/// failure is logged and does not mask the original error) before returning
/// -- so hardware never ends up holding a filter this crate has no
/// `MessageFilterId` on record for. On success, returns the installed ids
/// grouped by the client-supplied `FilterNumber` (a single `FilterNumber` can
/// map to more than one id, one per applicable `TxFlags` variant).
///
/// `filters` must already be validated by the caller (recognized,
/// non-`PDU_FLT_UNSPECIFIED` `filter_type`; well-formed
/// `filter_mask_message`/`filter_pattern_message`) -- this performs native
/// installs only, it does not re-validate shape (see `InstallFilterFailure`).
///
/// **ADR-162 invariant:** both call sites (`rpc_misc.rs`'s
/// `ioctl_start_msg_filter`, and this file's own pre-connect
/// `pending_client_filters` install) reject any request whose CLL's
/// `base_hw_protocol_id() == ISO15765` before either ever reaches this
/// function (ADR-038), so `hw_protocol_id` here is never ISO15765-family
/// today -- the `PassThruMessage::new` calls below tag every filter message
/// with the raw `hw_protocol_id` as-is, which would be the WRONG id for a
/// native-mixed channel's `PASS_FILTER`/`BLOCK_FILTER` (clause 8.2.2.4 needs
/// `resources::mixed_format_can_protocol_id`'s paired raw-CAN id instead,
/// exactly like the internal UUDT `PASS_FILTER` install in this file
/// already does). If a future change relaxes the `rpc_misc.rs` guard to
/// admit native-mixed client filters, it must also add that translation
/// here -- the debug assertion below exists to catch a relaxation that
/// forgets to.
pub(super) async fn install_client_message_filters(
    api: &J2534Api0404,
    cll_handle: u32,
    channel_id: ChannelId,
    hw_protocol_id: u32,
    connect_flags: u32,
    filters: &[vci_service_interface::IoFilter],
) -> Result<HashMap<u32, Vec<MessageFilterId>>, InstallFilterFailure> {
    debug_assert_ne!(
        resources::base_protocol_id(hw_protocol_id),
        j2534_0404::ISO15765,
        "install_client_message_filters must not be reached with an ISO15765-family \
         hw_protocol_id -- see this function's ADR-162 invariant note"
    );
    // ADR-157 Plane B: same normalization as `install_pass_all_filter` --
    // the message-building calls below still use the raw `hw_protocol_id`
    // (Plane A).
    let tx_flags = can_filter_tx_flags(resources::base_protocol_id(hw_protocol_id), connect_flags);
    let mut installed: Vec<(u32, MessageFilterId)> = Vec::with_capacity(filters.len());
    let install_result: Result<(), InstallFilterFailure> = (|| {
        for filter in filters {
            let filter_type = vci_service_interface::PduFilter::try_from(filter.filter_type)
                .map_err(|_| {
                    InstallFilterFailure::Status(Status::invalid_argument(format!(
                        "unrecognized PDU_FLT filter_type {}",
                        filter.filter_type
                    )))
                })?;
            let j2534_filter_type = match filter_type {
                vci_service_interface::PduFilter::PduFltPass
                | vci_service_interface::PduFilter::PduFltPassUudt => j2534_0404::PASS_FILTER,
                vci_service_interface::PduFilter::PduFltBlock
                | vci_service_interface::PduFilter::PduFltBlockUudt => j2534_0404::BLOCK_FILTER,
                vci_service_interface::PduFilter::PduFltUnspecified => {
                    return Err(InstallFilterFailure::Status(Status::invalid_argument(
                        "PDU_ERR_INVALID_PARAMETERS: filter_type is required",
                    )));
                }
            };
            for &flags in tx_flags {
                // ADR-186 (mirrors `ioctl_start_repeat_message`'s own round-15
                // precedent, ADR-165 PR #42, `rpc_misc.rs`): SAE J2534-2
                // 21.4.4 lets a device cap a PASSTHRU_MSG's DataSize at 12
                // bytes when TX_FD_CAN_FORMAT is unset -- a client-supplied
                // filter mask/pattern template longer than 8 payload bytes on
                // an FD-connected link needs this flag set for template
                // VALIDITY, even though clause 21.2.2(g) requires FD-capable-
                // channel filtering to ignore CAN message format entirely
                // (matching is on address+data only, never on classic-vs-FD
                // wire encoding -- unaffected either way). Applied
                // unconditionally for any FD-connected link's filters, not
                // length-gated: a short template stays valid either way, so
                // there is no reason to narrow this further. Deliberately NOT
                // TX_FD_CAN_BRS -- Table 93 defines BRS as a transmission
                // bit-timing property within an FD frame, not a classic/FD
                // discriminator, with no analogous DataSize-validity coupling
                // on a never-transmitted template (same reasoning round-15's
                // precedent already established). `can_filter_tx_flags`
                // itself is untouched -- its own contract is ID-width-variant
                // selection, orthogonal to this flag -- so the bit is ORed in
                // here instead, at both call sites below via this shared
                // `flags` binding.
                let flags = if resources::is_fd_protocol_id(hw_protocol_id) {
                    flags | j2534_0404::TX_FD_CAN_FORMAT
                } else {
                    flags
                };
                let mut mask = j2534_0404::PassThruMessage::new(
                    hw_protocol_id,
                    0,
                    flags,
                    0,
                    0,
                    &filter.filter_mask_message,
                )
                .map_err(|err| {
                    InstallFilterFailure::Status(Status::invalid_argument(format!(
                        "invalid filter_mask_message: {err}"
                    )))
                })?;
                let mut pattern = j2534_0404::PassThruMessage::new(
                    hw_protocol_id,
                    0,
                    flags,
                    0,
                    0,
                    &filter.filter_pattern_message,
                )
                .map_err(|err| {
                    InstallFilterFailure::Status(Status::invalid_argument(format!(
                        "invalid filter_pattern_message: {err}"
                    )))
                })?;
                let filter_id = api
                    .start_message_filter(
                        channel_id,
                        j2534_filter_type,
                        &mut mask,
                        &mut pattern,
                        None,
                    )
                    .map_err(InstallFilterFailure::Native)?;
                installed.push((filter.filter_number, filter_id));
            }
        }
        Ok(())
    })();

    if let Err(failure) = install_result {
        for &(filter_number, filter_id) in &installed {
            if let Err(stop_err) = api.stop_message_filter(channel_id, filter_id) {
                warn!(
                    cll_handle,
                    filter_number,
                    filter_id = filter_id.0,
                    %stop_err,
                    "install_client_message_filters rollback: stop_message_filter failed"
                );
            }
        }
        return Err(failure);
    }

    let mut by_filter_number: HashMap<u32, Vec<MessageFilterId>> = HashMap::new();
    for (filter_number, filter_id) in installed {
        by_filter_number
            .entry(filter_number)
            .or_default()
            .push(filter_id);
    }
    Ok(by_filter_number)
}

/// Decoded `CP_Can*Format` UNUM32 bitfield (ISO 22900-2 Table B.13: CAN ID
/// format for ISO_15765 / ISO_11898). Only the bits relevant to building a
/// `FLOW_CONTROL_FILTER` message are interpreted here:
///
/// - Bits 5,4 (Padding Overwrite) are TX-only and apply solely to
///   `CP_CanPhysReqFormat` / `CP_CanFuncReqFormat` outgoing-frame padding —
///   unrelated to filter construction, not read.
/// - Bit 3 (Addressing Scheme): 0 = normal (4-byte CAN Id), 1 = extended
///   (5-byte: CAN Id + a target-address-extension byte).
/// - Bit 2 (Data Transfer Handling: UUDT vs. USDT) is not read here — this
///   service picks the USDT-vs-UUDT filter to build from the `CP_*` param
///   name itself (`CP_CanRespUSDTId` vs. `CP_CanRespUUDTId`), not from this
///   bit, so it would be redundant with that choice (ADR-041).
/// - Bit 1 (CAN Id Size): 0 = 11-bit, 1 = 29-bit.
/// - Bit 0 (Flow Control): 0 = flow control frames disabled for this address,
///   1 = enabled. Only consulted for `CP_CanRespUSDTFormat` (skips the USDT
///   filter when clear, ADR-040); `CP_CanRespUUDTFormat`'s bit 0 is not read
///   — flow control is inherently not applicable to unsegmented UUDT
///   addressing, so the UUDT filter is always installed when
///   `CP_CanRespUUDTId` is present (ADR-041).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CanIdFormat {
    extended_addressing: bool,
    extended_can_id: bool,
    flow_control_enabled: bool,
}

impl CanIdFormat {
    fn from_raw(raw: u32) -> Self {
        Self {
            extended_addressing: raw & 0x08 != 0,
            extended_can_id: raw & 0x02 != 0,
            flow_control_enabled: raw & 0x01 != 0,
        }
    }

    /// `PASSTHRU_MSG.TxFlags` bits implied by this format, for the mask/
    /// pattern/flow-control messages built by `can_filter_message`.
    ///
    /// `filter_type` gates `ISO15765_ADDR_TYPE`: per SAE J2534-1 Table B.13,
    /// this bit is an ISO15765-protocol extended-addressing indicator and is
    /// only meaningful on a `FLOW_CONTROL_FILTER`'s ISO15765 messages -- it
    /// must never be set on a raw-CAN `PASS_FILTER`/`BLOCK_FILTER` message
    /// (e.g. the ADR-160/Phase 3c native-mixed UUDT alternative), since a
    /// conforming adapter may reject such a filter outright (Codex review,
    /// PR #32). `TX_EXTENDED_ID` is unaffected -- it selects 11- vs. 29-bit
    /// CAN Id width and applies regardless of `filter_type`.
    fn tx_flags(self, filter_type: u32) -> u32 {
        let mut flags = 0;
        if self.extended_can_id {
            flags |= j2534_0404::TX_EXTENDED_ID;
        }
        if self.extended_addressing && filter_type == j2534_0404::FLOW_CONTROL_FILTER {
            flags |= j2534_0404::ISO15765_ADDR_TYPE;
        }
        flags
    }
}

impl Default for CanIdFormat {
    /// Used when a `CP_Can*Format` param is absent from a `UniqueRespIdTable`
    /// entry: normal 11-bit addressing with flow control enabled — the
    /// behavior this service had before Table B.13 decoding was added, so
    /// entries that only set `CP_CanRespUSDTId` / `CP_CanPhysReqId` (no
    /// format) keep working unchanged.
    fn default() -> Self {
        Self {
            extended_addressing: false,
            extended_can_id: false,
            flow_control_enabled: true,
        }
    }
}

/// `PassThruConnect` `Flags` bits (J2534-1 v04.04) implied by the creator
/// CLL's Working ComParam set and its UniqueRespIdTable as configured at
/// connect time (ADR-065).
///
/// For `CAN`: always `CAN_ID_BOTH` plus `CAN_29BIT_ID` iff the
/// physical-request address is 29-bit (`phys_req_extended`) — raw-CAN
/// channels in this service are wide-open receive channels (pass-all filter
/// plus service-side `UniqueRespIdTable` matching in `poll_rx`) shared by
/// `(CAN, baud)` alone, so the set of CAN-ID widths a channel will ever carry
/// is unknowable at connect time: a later raw-CAN CLL with a different width
/// may join the same `(CAN, baud)` channel, and the ADR-046 UUDT companion
/// may reuse it in either creation order. `CAN_ID_BOTH` is therefore the only
/// safe value for `CAN`; bit 8 still conveys the prioritized type from the
/// creator's physical-request format. For `ISO15765`: `CAN_29BIT_ID`/
/// `CAN_ID_BOTH` from Table B.13's bit 1 across the configured CAN addresses
/// (`can_connect_flags`) — ISO15765 channels are shared by the same key, but
/// their steady-state filters are point-to-point `FLOW_CONTROL_FILTER`s
/// built from the exact CAN ID (ADR-048), so there is no wide-open receive
/// path for a width mismatch to hide in and the observation-based derivation
/// is safe (a joining CLL whose width differs from the creator's remains the
/// documented client-side responsibility, ADR-065). For `ISO9141`/
/// `ISO14230`: `ISO9141_K_LINE_ONLY` from `CP_K_L_LineInit`, ORed with
/// `ISO9141_NO_CHECKSUM` (SAE J2534-1 v04.04 §7.2.3.3 Figure 7's connect `Flags` bit
/// 9) iff `raw_mode && !checksum_mode` for the connecting CLL (ADR-198 Phase
/// 2, superseding this doc comment's prior "deliberately never set" rule for
/// that bit -- see ADR-065's own Status line). Every other
/// protocol gets 0: no v04.04 connect flag this service derives applies to
/// them. The result is fixed at connect time and is not revisited if the
/// UniqueRespIdTable or ComParams change afterward (ADR-044's shared-channel,
/// creator-decides rule extends to these flags).
///
/// `raw_mode`/`checksum_mode` (ADR-198 Phase 2): the connecting CLL's own
/// `LogicalLinkState::raw_mode`/`checksum_mode` -- consulted only by the
/// `ISO9141`/`ISO14230` arm; every other arm ignores them (RawMode has no
/// connect-flag-visible effect for CAN/ISO15765/J1939/Ethernet_NDIS in this
/// or any prior phase).
fn connect_flags(
    j2534_proto_id: u32,
    working: &ComParamSet,
    urid_table: &[EcuUniqueRespEntry],
    raw_mode: bool,
    checksum_mode: bool,
) -> u32 {
    match j2534_proto_id {
        j2534_0404::CAN => {
            j2534_0404::CAN_ID_BOTH
                | if phys_req_extended(working, urid_table) {
                    j2534_0404::CAN_29BIT_ID
                } else {
                    0
                }
        }
        j2534_0404::ISO15765 => can_connect_flags(working, urid_table),
        // Round-18 fix (Codex review, PR #72): SAE J1939-21's 29-bit
        // extended CAN identifier is unconditional -- unlike plain CAN or
        // ISO15765, there is no client-configurable addressing-format
        // ComParam to derive this from (clause 16's own message set is
        // fixed-width), so this arm is a flat `CAN_29BIT_ID`, never
        // `CAN_ID_BOTH` (accepting 11-bit frames too would be wrong for a
        // protocol that never sends them). `j2534_proto_id` here is
        // `base_hw_protocol_id()` (ADR-157 Plane B), and `PROTOCOL_J1939_PS`
        // self-maps under `resources::base_protocol_id` (no separate base
        // J1939 id exists), so this arm's own constant is the `_PS` id
        // itself, matching this file's own established precedent for
        // Honda DIAG-H's identical self-mapping.
        j2534_0404::PROTOCOL_J1939_PS => j2534_0404::CAN_29BIT_ID,
        // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): resolves
        // the staged `CP_NdisPinOption` ComParam into the native connect
        // flag bits at `ConnectComLogicalLink` time.
        j2534_0404::PROTOCOL_ETHERNET_NDIS => ndis_pin_option_connect_flags(working),
        j2534_0404::ISO9141 | j2534_0404::ISO14230 => {
            let k_line_only = if working
                .unum32
                .get(&PARAM_K_L_LINE_INIT)
                .copied()
                .unwrap_or(0)
                != 0
            {
                j2534_0404::ISO9141_K_LINE_ONLY
            } else {
                0
            };
            // ADR-198 Phase 2 Decision item 2: the manual-checksum bit is
            // set iff RawMode is ON AND ChecksumMode is OFF for this CLL --
            // ISO 22900-2:2022 Table D.6's "interface still manages the
            // checksum under ChecksumMode=ON" rule means the native flag
            // stays clear in that case, matching RawMode=OFF's own
            // interface-managed behavior.
            let no_checksum = if raw_mode && !checksum_mode {
                j2534_0404::ISO9141_NO_CHECKSUM
            } else {
                0
            };
            k_line_only | no_checksum
        }
        _ => 0,
    }
}

/// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): resolves the
/// staged `CP_NdisPinOption` ComParam (`PARAM_NDIS_PIN_OPTION`, UNUM32; `0`
/// = auto/default, `1` = Option 1, `2` = Option 2) into the native connect
/// flag bits `PassThruConnect` accepts -- `CONNECT_FLAG_NDIS_PINS_OPTION1`/
/// `_OPTION2`, mutually exclusive. The native table treats both bits set as
/// equivalent to neither, so auto (`0`, and any other unrecognized value) is
/// encoded as neither bit -- never both.
fn ndis_pin_option_connect_flags(working: &ComParamSet) -> u32 {
    match working
        .unum32
        .get(&PARAM_NDIS_PIN_OPTION)
        .copied()
        .unwrap_or(0)
    {
        1 => j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION1,
        2 => j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION2,
        _ => 0,
    }
}

/// ISO 22900-2 Table B.20's sentinel for `CP_CanRespUUDTId`: `0xFFFFFFFF`
/// means "not used" even when the ComParam key is present in the entry. The
/// key is always present in an unedited copy of `GetUniqueRespIdTable`'s
/// template (the spec's own documented `SetUniqueRespIdTable` workflow), so
/// callers that gate UUDT filter/companion-channel setup on this ID must use
/// [`uudt_resp_id`] rather than raw key presence (A2-12 Codex follow-up).
const CAN_RESP_UUDT_ID_UNUSED: u32 = 0xFFFF_FFFF;

/// `entry`'s configured `CP_CanRespUUDTId`, treating the spec's
/// [`CAN_RESP_UUDT_ID_UNUSED`] sentinel as absent rather than as a real
/// address to filter on.
pub(super) fn uudt_resp_id(entry: &EcuUniqueRespEntry) -> Option<u32> {
    entry
        .params
        .unum32
        .get(&PARAM_CAN_RESP_UUDT_ID)
        .copied()
        .filter(|&id| id != CAN_RESP_UUDT_ID_UNUSED)
}

/// ISO 22900-2's identical `0xFFFFFFFF` "not used" sentinel for
/// `CP_CanRespUSDTId` (same Table 76 semantics as [`CAN_RESP_UUDT_ID_UNUSED`]
/// two entries later in the spec's own ComParam table -- the two ComParams
/// share this sentinel value; only their PRESET default typically differs,
/// which is an authoring convention, not a protocol invariant). Confirmed
/// via direct verification against both available spec editions (Codex
/// review, ADR-165 PR #42 round 19 follow-up).
const CAN_RESP_USDT_ID_UNUSED: u32 = 0xFFFF_FFFF;

/// `entry`'s configured `CP_CanRespUSDTId`, treating the spec's
/// [`CAN_RESP_USDT_ID_UNUSED`] sentinel as absent rather than as a real
/// address to filter on -- mirrors [`uudt_resp_id`] exactly, for the
/// ISO15765 counterpart ComParam.
pub(super) fn usdt_resp_id(entry: &EcuUniqueRespEntry) -> Option<u32> {
    entry
        .params
        .unum32
        .get(&PARAM_CAN_RESP_USDT_ID)
        .copied()
        .filter(|&id| id != CAN_RESP_USDT_ID_UNUSED)
}

/// Whether `entry`'s USDT/UUDT field would ever actually receive a
/// point-to-point `FLOW_CONTROL_FILTER`/`PASS_FILTER`
/// (`install_point_to_point_fc_filters`) on native ISO15765/native-mixed
/// hardware -- i.e. whether that field could ever legitimately receive a
/// frame of its own configured width at all (ADR-222 round 6 gap: a table
/// entry with no filter installed for a field can never contribute a real
/// width observation for that field, and treating it as if it could can
/// falsely mark an unrelated, properly-filtered entry's same numeric id as
/// contended).
pub(super) struct PointToPointFilterEligibility {
    pub(super) usdt: bool,
    pub(super) uudt: bool,
}

/// Mirrors `install_point_to_point_fc_filters`'s own per-entry go/no-go
/// decision EXACTLY -- this is the single source of truth for that
/// decision; `install_point_to_point_fc_filters` itself calls this function
/// rather than re-deriving it, so there is exactly one place this logic is
/// defined (ADR-217-round-2 "two searches disagree" discipline).
///
/// USDT: eligible iff `usdt_resp_id(entry)` is configured (not the
/// `0xFFFFFFFF` sentinel), the entry's `CP_CanRespUSDTFormat` has flow
/// control enabled (bit 0 -- absent Format defaults to enabled, per
/// `CanIdFormat::default()`), and the entry configures a `CP_CanPhysReqId`
/// (any value; only key presence matters here, the same as
/// `install_point_to_point_fc_filters`'s own `req` gate).
///
/// UUDT: eligible iff `uudt_resp_id(entry)` is configured, `!dual_channel`
/// (in dual-channel mode UUDT is received on the ADR-046 companion channel
/// instead, so no point-to-point filter is ever installed on the primary
/// for it), and either `native_mixed` (a native-mixed `PASS_FILTER` is
/// installed unconditionally, regardless of `CP_CanPhysReqId`) or the entry
/// configures a `CP_CanPhysReqId` (the ADR-041 `FLOW_CONTROL_FILTER`
/// workaround requires it). UUDT's own Format's flow-control bit is never
/// consulted here -- `install_point_to_point_fc_filters` never reads it for
/// the UUDT branch either (ADR-041: flow control is not applicable to
/// unsegmented UUDT addressing).
pub(super) fn point_to_point_filter_eligibility(
    entry: &EcuUniqueRespEntry,
    dual_channel: bool,
    native_mixed: bool,
) -> PointToPointFilterEligibility {
    let has_req = entry.params.unum32.contains_key(&PARAM_CAN_PHYS_REQ_ID);

    let usdt = usdt_resp_id(entry).is_some() && {
        let resp_format = entry
            .params
            .unum32
            .get(&PARAM_CAN_RESP_USDT_FORMAT)
            .map_or_else(CanIdFormat::default, |&raw| CanIdFormat::from_raw(raw));
        resp_format.flow_control_enabled && has_req
    };

    let uudt = uudt_resp_id(entry).is_some() && !dual_channel && (native_mixed || has_req);

    PointToPointFilterEligibility { usdt, uudt }
}

/// `entry`'s `format_id` Table B.13 bit 1 (29-bit CAN Id), or the
/// `CanIdFormat::default()` 11-bit fallback when `format_id` is absent from
/// the entry — the same default `install_point_to_point_fc_filters` applies
/// to a configured address with no format param.
fn entry_format_extended(entry: &EcuUniqueRespEntry, format_id: ComParamId) -> bool {
    entry.params.unum32.get(&format_id).map_or_else(
        || CanIdFormat::default().extended_can_id,
        |&raw| CanIdFormat::from_raw(raw).extended_can_id,
    )
}

/// Whether the physical-request CAN address is 29-bit: the entry-level
/// `CP_CanPhysReqFormat` of the first `UniqueRespIdTable` entry that
/// configures a `CP_CanPhysReqId`, or — when no entry does — the link-level
/// Working `CP_CanPhysReqFormat`. Shared by `connect_flags`'s `CAN` case
/// (the priority bit alongside an unconditional `CAN_ID_BOTH`) and
/// `can_connect_flags`'s `ISO15765` derivation (the same priority-bit rule
/// applied on top of its observation-based `CAN_ID_BOTH`/`CAN_29BIT_ID`).
fn phys_req_extended(working: &ComParamSet, urid_table: &[EcuUniqueRespEntry]) -> bool {
    urid_table
        .iter()
        .find(|entry| entry.params.unum32.contains_key(&PARAM_CAN_PHYS_REQ_ID))
        .map(|entry| entry_format_extended(entry, PARAM_CAN_PHYS_REQ_FORMAT))
        .unwrap_or_else(|| {
            working
                .unum32
                .get(&PARAM_CAN_PHYS_REQ_FORMAT)
                .is_some_and(|&raw| CanIdFormat::from_raw(raw).extended_can_id)
        })
}

/// `ISO15765` half of `connect_flags`: derives `CAN_29BIT_ID`/`CAN_ID_BOTH`
/// from the CAN-ID-type observations configured so far. Unlike the `CAN`
/// case, an ISO15765 channel's steady-state filters are point-to-point
/// `FLOW_CONTROL_FILTER`s built from the exact CAN ID (ADR-048), so deriving
/// the connect flags from what is actually configured — rather than always
/// assuming `CAN_ID_BOTH` — is safe here.
///
/// Observations are collected per `UniqueRespIdTable` entry — mirroring the
/// FLOW_CONTROL_FILTER address gating in `install_point_to_point_fc_filters`:
/// `CP_CanPhysReqFormat` counts only when the entry has `CP_CanPhysReqId`,
/// `CP_CanRespUSDTFormat`/`CP_CanRespUUDTFormat` only when their respective
/// `Id` is present and not the spec's `0xFFFFFFFF` "not used" sentinel (see
/// [`usdt_resp_id`]/[`uudt_resp_id`] — both ComParams share the identical
/// sentinel semantics, Codex review PR #42 round 19 follow-up; a prior
/// version of this comment claimed the sentinel filter was UUDT-specific,
/// which was itself the bug — the USDT branch used a raw presence check
/// instead of [`usdt_resp_id`]). If the table contributed no observations at all (empty,
/// or no entry carries any of the three IDs), the link-level Working
/// `CP_CanPhysReqFormat`/`CP_CanRespUSDTFormat`/`CP_CanRespUUDTFormat` are
/// used instead. `CP_CanFuncReqFormat` is link-level only (functional
/// requests are not per-entry) and is always additionally consulted.
///
/// `CAN_ID_BOTH` is set when `CP_CanMixedFormat` is nonzero or the
/// observations disagree (some 11-bit, some 29-bit); its priority bit
/// (`CAN_29BIT_ID`) then follows the physical-request address specifically
/// (`phys_req_extended`: entry-level if any entry configures one, else the
/// link-level format). Otherwise `CAN_29BIT_ID` alone is set when every
/// observation is 29-bit.
fn can_connect_flags(working: &ComParamSet, urid_table: &[EcuUniqueRespEntry]) -> u32 {
    let mut observations: Vec<bool> = Vec::new();
    for entry in urid_table {
        if entry.params.unum32.contains_key(&PARAM_CAN_PHYS_REQ_ID) {
            observations.push(entry_format_extended(entry, PARAM_CAN_PHYS_REQ_FORMAT));
        }
        if usdt_resp_id(entry).is_some() {
            observations.push(entry_format_extended(entry, PARAM_CAN_RESP_USDT_FORMAT));
        }
        if uudt_resp_id(entry).is_some() {
            observations.push(entry_format_extended(entry, PARAM_CAN_RESP_UUDT_FORMAT));
        }
    }
    if observations.is_empty() {
        for format_id in [
            PARAM_CAN_PHYS_REQ_FORMAT,
            PARAM_CAN_RESP_USDT_FORMAT,
            PARAM_CAN_RESP_UUDT_FORMAT,
        ] {
            if let Some(&raw) = working.unum32.get(&format_id) {
                observations.push(CanIdFormat::from_raw(raw).extended_can_id);
            }
        }
    }
    if let Some(&raw) = working.unum32.get(&PARAM_CAN_FUNC_REQ_FORMAT) {
        observations.push(CanIdFormat::from_raw(raw).extended_can_id);
    }

    let mixed = working
        .unum32
        .get(&PARAM_CAN_MIXED_FORMAT)
        .copied()
        .unwrap_or(0)
        != 0
        || (observations.contains(&true) && observations.contains(&false));

    let extended_primary = phys_req_extended(working, urid_table);

    if mixed {
        j2534_0404::CAN_ID_BOTH
            | if extended_primary {
                j2534_0404::CAN_29BIT_ID
            } else {
                0
            }
    } else if observations.contains(&true) {
        j2534_0404::CAN_29BIT_ID
    } else {
        0
    }
}

/// A resolved CAN address (ID + Table B.13 format + extension-address byte)
/// used to build one side of a point-to-point `FLOW_CONTROL_FILTER`.
///
/// `PartialEq`/`Eq` (ADR-162 Decision 2) compare the full match key -- id,
/// format (width/extended-addressing bits), and `ext_addr` -- not just the
/// bare numeric `id`: a bare-id comparison would both over-reject an 11-bit/
/// 29-bit numeric coincidence and under-specify extended addressing. Used by
/// [`find_native_mixed_uudt_usdt_collision`] to compare a `CP_CanRespUUDTId`
/// match key against a `CP_CanRespUSDTId` match key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CanAddress {
    id: u32,
    format: CanIdFormat,
    /// Low byte of `CP_Can{PhysReq,RespUSDT}ExtAddr`; only meaningful (and only
    /// appended to the filter message) when `format.extended_addressing`.
    ext_addr: u8,
}

/// Builds the `pMaskMsg` (`is_mask = true`) or `pPatternMsg`/`pFlowControlMsg`
/// (`is_mask = false`) message for `addr`: 4 bytes (big-endian CAN Id) plus a
/// 5th extension-address byte when `addr.format.extended_addressing` is set.
/// The mask byte for the extension position is `$FF` like the rest of the mask.
///
/// `protocol_id` is the ISO15765 channel's actual connect-time `hw_protocol_id`
/// (ADR-157 Plane A: the raw, e.g. `ISO15765_PS`, id -- NOT a normalized/base
/// id) -- the message's `ProtocolID` field must match the literal id the
/// channel was opened with, or a conforming adapter may reject the filter
/// with `ERR_MSG_PROTOCOL_ID` (found by Codex review, PR #28).
///
/// `filter_type` is the native `PassThruStartMsgFilter` filter type this
/// message is built for (`FLOW_CONTROL_FILTER` or `PASS_FILTER`/
/// `BLOCK_FILTER`) and is passed straight through to `CanIdFormat::tx_flags`
/// so the ISO15765-only `ISO15765_ADDR_TYPE` `TxFlags` bit is never set on a
/// raw-CAN message (Codex review, PR #32). The 5th extension-address data
/// byte `addr.format.extended_addressing` appends below is unaffected by
/// `filter_type` -- it is a plain CAN data byte, valid regardless of
/// protocol.
fn can_filter_message(
    addr: &CanAddress,
    is_mask: bool,
    protocol_id: u32,
    filter_type: u32,
) -> j2534_0404::PassThruMessage {
    let mut data = if is_mask {
        vec![0xFFu8; 4]
    } else {
        addr.id.to_be_bytes().to_vec()
    };
    if addr.format.extended_addressing {
        data.push(if is_mask { 0xFF } else { addr.ext_addr });
    }
    j2534_0404::PassThruMessage::new(
        protocol_id,
        0,
        addr.format.tx_flags(filter_type),
        0,
        0,
        &data,
    )
    .expect("mask/pattern/flow-control filter data is always 4 or 5 bytes")
}

/// Installs a spec-conformant point-to-point `FLOW_CONTROL_FILTER` for a single
/// ECU response address, per the J2534 v04.04 spec for `PassThruStartMsgFilter`:
///
/// - `pMaskMsg`: `$FF` bytes — flow control filters are point-to-point and
///   must not match multiple CAN identifiers.
/// - `pPatternMsg`: the actual CAN ID at the other end of the conversation
///   (`resp` — either `CP_CanRespUSDTId`/`CP_CanRespUSDTFormat`/
///   `CP_CanRespUSDTExtAddr` for a segmented USDT response, or
///   `CP_CanRespUUDTId`/`CP_CanRespUUDTFormat`/`CP_CanRespUUDTExtAddr` for an
///   unsegmented UUDT response (ADR-041) — the ECU's physical response
///   address either way).
/// - `pFlowControlMsg`: the actual CAN ID used when sending CAN frames on this
///   ECU's physical request (`req` — `CP_CanPhysReqId`/`CP_CanPhysReqFormat`/
///   `CP_CanPhysReqExtAddr`, matched against the CAN ID in a segmented
///   `PassThruWriteMsg`; also required as a non-null `pFlowControlMsg` for the
///   UUDT filter even though UUDT itself never triggers flow control frames —
///   see ADR-041).
///
/// `pMaskMsg` and `pPatternMsg` are always built from `resp`'s format (their
/// `DataSize` must match each other); `pFlowControlMsg` is built independently
/// from `req`'s format, since it is a separate message and may use a different
/// addressing scheme.
///
/// `protocol_id` is the id placed in every mask/pattern/flow-control message
/// built here -- normally the channel's actual connect-time `hw_protocol_id`
/// (ADR-157 Plane A, raw -- e.g. `ISO15765_PS` for a pin-selected link), but
/// for a `PASS_FILTER`/`BLOCK_FILTER` under ADR-160/Phase 3c native-mixed
/// mode the caller has already substituted the paired raw-CAN id
/// (`resources::mixed_format_can_protocol_id`, SAE J2534-2 clause 8.2.2.4) --
/// this function never performs that substitution itself.
///
/// `filter_type` is `j2534_0404::FLOW_CONTROL_FILTER` for the conformant
/// USDT case and the ADR-041 UUDT workaround, or `j2534_0404::PASS_FILTER`
/// for the ADR-160/Phase 3c native-mixed UUDT alternative (Decision 4): a
/// `PASS_FILTER`/`BLOCK_FILTER` has no `pFlowControlMsg` (the third message
/// is `FLOW_CONTROL_FILTER`-only, per the J2534 v04.04 spec for
/// `PassThruStartMsgFilter`), so `req` is only consulted -- and `flow_control`
/// only built -- when `filter_type` is `FLOW_CONTROL_FILTER`.
///
/// `req` is therefore `Option`: a `FLOW_CONTROL_FILTER` call site must always
/// pass `Some` (a `pFlowControlMsg` is mandatory for that filter type; passing
/// `None` there is a caller bug, hence the `expect` below), while the
/// native-mixed `PASS_FILTER` call site may pass `None` for a UUDT-only
/// `UniqueRespIdTable` entry that carries no `CP_CanPhysReqId` (Codex review,
/// PR #32 -- `SetUniqueRespIdTable` does not require a request address to
/// accompany a UUDT response id).
fn install_point_to_point_fc_filter(
    api: &J2534Api0404,
    channel_id: ChannelId,
    filter_type: u32,
    resp: &CanAddress,
    req: Option<&CanAddress>,
    protocol_id: u32,
) -> Result<MessageFilterId, j2534_0404::Error> {
    let mut mask = can_filter_message(resp, true, protocol_id, filter_type);
    let mut pattern = can_filter_message(resp, false, protocol_id, filter_type);
    if filter_type == j2534_0404::FLOW_CONTROL_FILTER {
        let req = req.expect(
            "FLOW_CONTROL_FILTER call sites always pass a request address (pFlowControlMsg is mandatory for this filter type)",
        );
        let mut flow_control = can_filter_message(req, false, protocol_id, filter_type);
        api.start_message_filter(
            channel_id,
            filter_type,
            &mut mask,
            &mut pattern,
            Some(&mut flow_control),
        )
    } else {
        api.start_message_filter(channel_id, filter_type, &mut mask, &mut pattern, None)
    }
}

/// True when `a` and `b`'s native `PassThruStartMsgFilter` mask/pattern byte
/// sequences (`can_filter_message`, this file) can both match the SAME
/// physical CAN frame -- i.e. their hardware filters overlap, not merely
/// whether their `CanAddress` fields are equal (Codex review, PR #35: an
/// equality-based check misses a real overlap between a normal-addressed and
/// an extended-addressed key at the same id).
///
/// `id` and `format.extended_can_id` (11-/29-bit width) must match exactly:
/// a different numeric id is trivially a different frame, and 11-bit vs.
/// 29-bit at the same numeric id are structurally different CAN frames (the
/// IDE bit; `TX_EXTENDED_ID` is set unconditionally on width by
/// `CanIdFormat::tx_flags`, matching this codebase's existing width-aliasing
/// treatment elsewhere, e.g. `events.rs`'s `isotp_target_collision`) -- never
/// the same physical frame regardless of any other field.
///
/// `format.extended_addressing`/`ext_addr` do NOT need to match:
/// `can_filter_message` emits a 4-byte mask/pattern (ID only) for a
/// normal-addressed key, matching ANY frame with that id regardless of data
/// content, and only appends a 5th `ext_addr` byte -- narrowing the match to
/// frames whose first data byte equals it -- when `extended_addressing` is
/// set. A normal-addressed filter is therefore a strict superset of any
/// extended-addressed filter at the same id: every frame the extended one
/// would match, the normal one also matches, so they always overlap
/// regardless of `ext_addr`. Two extended-addressed keys at the same id only
/// overlap when their `ext_addr` also matches -- their 5-byte patterns are
/// otherwise mutually exclusive (a single frame's first data byte cannot
/// equal two different values at once).
fn can_filter_keys_overlap(a: &CanAddress, b: &CanAddress) -> bool {
    if a.id != b.id || a.format.extended_can_id != b.format.extended_can_id {
        return false;
    }
    if a.format.extended_addressing && b.format.extended_addressing {
        a.ext_addr == b.ext_addr
    } else {
        true
    }
}

/// ADR-162 Decision 2: detects a native-mixed-mode `CP_CanRespUUDTId`/
/// `CP_CanRespUSDTId` hardware-filter overlap (see [`can_filter_keys_overlap`])
/// across every `UniqueRespIdTable` active on the same physical channel (a
/// candidate table plus zero or more sibling tables, pooled together in
/// `tables`). Under `CAN_MIXED_FORMAT_ON` (SAE J2534-2 clause 8), a UUDT key
/// whose filter overlaps a flow-control-eligible USDT key's filter is always
/// captured by the `FLOW_CONTROL_FILTER` and never reaches the native
/// `PASS_FILTER` this service installs for it
/// (`install_point_to_point_fc_filters`) -- a deterministic misrouting/
/// corruption hazard.
///
/// A USDT key participates only when it is actually installed as a
/// `FLOW_CONTROL_FILTER` by `install_point_to_point_fc_filters`: `CP_CanRespUSDTId`
/// present and not the spec's `0xFFFFFFFF` "not used" sentinel (see
/// [`usdt_resp_id`]), its `CP_CanRespUSDTFormat`'s flow-control bit set, AND
/// `CP_CanPhysReqId` present (mirroring that function's own `req.is_none()`
/// warn-and-skip at ~3140-3151 -- a USDT id with no request address never
/// gets a `FLOW_CONTROL_FILTER` installed, so it cannot collide either). A
/// UUDT key participates whenever [`uudt_resp_id`] returns `Some`. Both keys
/// are built via the same `CanAddress`/`CanIdFormat` construction
/// `install_point_to_point_fc_filters` uses, with their real `CP_Can*Format`
/// bits preserved -- `flow_control_enabled` is simply never consulted by
/// [`can_filter_keys_overlap`] (a USDT key's is always set by the eligibility
/// gate above; a UUDT key's is spec-meaningless per ADR-041), and `ext_addr`
/// need not be normalized since the overlap check already accounts for when
/// it does and doesn't matter. Self-collisions within one table (a single
/// entry setting both `CP_CanRespUSDTId` and `CP_CanRespUUDTId` to the same
/// value) are caught the same way, since `tables` may contain just one slice.
///
/// Returns the first colliding UUDT match key found (for the caller's error
/// message), or `None` if no overlap exists. Pure and synchronous -- no
/// locks, no `.await` -- so both connect-time
/// (`rpc_connect_com_logical_link`/`finalize_connected_link`) and promote-time
/// (`promote_unique_resp_id_table`) call sites can call it while already
/// holding `logical_links`.
fn find_native_mixed_uudt_usdt_collision(tables: &[&[EcuUniqueRespEntry]]) -> Option<CanAddress> {
    let mut usdt_keys: Vec<CanAddress> = Vec::new();
    let mut uudt_keys: Vec<CanAddress> = Vec::new();

    for &table in tables {
        for entry in table {
            if let Some(resp_id) = usdt_resp_id(entry) {
                let resp_format = entry
                    .params
                    .unum32
                    .get(&PARAM_CAN_RESP_USDT_FORMAT)
                    .map_or_else(CanIdFormat::default, |&raw| CanIdFormat::from_raw(raw));
                if resp_format.flow_control_enabled
                    && entry.params.unum32.contains_key(&PARAM_CAN_PHYS_REQ_ID)
                {
                    usdt_keys.push(CanAddress {
                        id: resp_id,
                        format: resp_format,
                        ext_addr: entry
                            .params
                            .unum32
                            .get(&PARAM_CAN_RESP_USDT_EXT_ADDR)
                            .copied()
                            .unwrap_or(0) as u8,
                    });
                }
            }

            if let Some(resp_id) = uudt_resp_id(entry) {
                let resp_format = entry
                    .params
                    .unum32
                    .get(&PARAM_CAN_RESP_UUDT_FORMAT)
                    .map_or_else(CanIdFormat::default, |&raw| CanIdFormat::from_raw(raw));
                uudt_keys.push(CanAddress {
                    id: resp_id,
                    format: resp_format,
                    ext_addr: entry
                        .params
                        .unum32
                        .get(&PARAM_CAN_RESP_UUDT_EXT_ADDR)
                        .copied()
                        .unwrap_or(0) as u8,
                });
            }
        }
    }

    uudt_keys.into_iter().find(|uudt| {
        usdt_keys
            .iter()
            .any(|usdt| can_filter_keys_overlap(uudt, usdt))
    })
}

/// Applies all J2534-standard unum32 params from `params` to the hardware
/// channel in a single `PassThruIoctl SET_CONFIG` call, skipping `skip` when set.
///
/// `hw_protocol_id` (ADR-158: the channel's actual, un-normalized -- ADR-157
/// Plane A -- hardware protocol id, e.g. `LogicalLinkState::hw_protocol_id`)
/// is passed straight through to `to_j2534_config_id`, so an FD_CAN_PS
/// channel's read-only bit-timing params (clause 21.3.2.5.1) are correctly
/// suppressed; `expand_tidle` below instead uses the normalized base id
/// (`resources::base_protocol_id`, ADR-157 Plane B) for its own
/// ISO9141/ISO14230 family-arm dispatch, unaffected by this change.
///
/// ComParam IDs with no `to_j2534_config_id()` translation for
/// `hw_protocol_id` (service-specific IDs, or native IDs not supported on
/// this protocol per ADR-028) are silently ignored. A `DATA_BITS` entry
/// originating from `CP_UartConfig` is split into `DATA_BITS` + `PARITY` by
/// `expand_uart_config` (ADR-071), and a `TIDLE` entry additionally derives
/// a `W0` (ISO9141) or `W5` (ISO14230) entry via `expand_tidle` (ADR-072),
/// before being sent. Both expansions run on still-unconverted (ISO 22900-2
/// microsecond) values -- `to_j2534_config_value` runs last, once per final
/// entry, so a derived entry is converted exactly once like every other.
fn apply_j2534_params(
    api: &J2534Api0404,
    channel_id: ChannelId,
    hw_protocol_id: u32,
    params: &ComParamSet,
    skip: Option<ComParamId>,
) -> Result<(), j2534_0404::Error> {
    let configs: Vec<(u32, u32)> = params
        .unum32
        .iter()
        .filter(|&(&param_id, _)| Some(param_id) != skip)
        .filter_map(|(&param_id, &value)| {
            let config_id = param_id.to_j2534_config_id(hw_protocol_id)?;
            Some((config_id, value))
        })
        .collect();
    let configs = expand_uart_config(configs);
    let configs = expand_tidle(configs, resources::base_protocol_id(hw_protocol_id));
    let configs: Vec<(u32, u32)> = configs
        .into_iter()
        .map(|(config_id, value)| (config_id, to_j2534_config_value(config_id, value)))
        .collect();
    if configs.is_empty() {
        return Ok(());
    }
    api.set_config(channel_id, &configs)
}

fn extract_bustype_name(
    resource: &vci_service_interface::create_com_logical_link_request::Resource,
) -> Option<&str> {
    if let vci_service_interface::create_com_logical_link_request::Resource::RscData(data) =
        resource
        && let Some(vci_service_interface::resource_data::BusType::BusTypeName(name)) =
            &data.bus_type
    {
        return Some(name.as_str());
    }
    None
}

fn extract_protocol_name(
    resource: &vci_service_interface::create_com_logical_link_request::Resource,
) -> Option<&str> {
    if let vci_service_interface::create_com_logical_link_request::Resource::RscData(data) =
        resource
        && let Some(vci_service_interface::resource_data::Protocol::ProtocolName(name)) =
            &data.protocol
    {
        return Some(name.as_str());
    }
    None
}

/// Resolves and validates `CreateComLogicalLinkRequest.cll_create_flag`
/// (ADR-196 Decision item 1): reads RawMode (ISO 22900-2:2022 Table D.6
/// byte 0 bit 7) from either representation -- `CllCreateFlagBit::
/// CllCreateFlagRawMode` in the named-bits form, or byte 0 bit 7 in the raw
/// byte-array form -- and rejects `Status::invalid_argument` for ANY other
/// nonzero bit: byte 0 bits 5-0, every byte beyond byte 0 in the raw form,
/// or an unrecognized/`UNSPECIFIED` named enum value in the bits form.
/// Table D.6 declares all of those Unused; this phase must not repeat, for
/// any bit it does not itself give behavior to, the silently-ignored-flag
/// defect it closes for bits 6/7 (ADR-196 Context).
///
/// ChecksumMode (`CllCreateFlagBit::CllCreateFlagChecksumMode`/byte 0 bit
/// 6) is read and format-validated the same way and returned alongside
/// RawMode as this function's second `bool` -- ADR-198 Phase 2: unlike
/// Phase 1 (which never stored it, since every Phase 1 RawMode-allowlisted
/// protocol has no checksum concept), K-line RawMode now gives ChecksumMode
/// real semantics (ISO 22900-2:2022 Table D.6), so its value must survive
/// past this function. It is meaningless whenever RawMode is OFF or for a
/// checksumless protocol, but is still resolved/validated unconditionally
/// here -- callers consult it only where it applies.
///
/// Returns `(raw_mode, checksum_mode)`. `None` (the request supplied
/// neither `cll_create_flag_bits` nor `cll_create_flag_raw`) resolves to
/// `(false, false)` -- the only values every pre-ADR-196 `CreateComLogicalLink`
/// caller ever got.
fn resolve_cll_create_flag(
    cll_create_flag: Option<vci_service_interface::create_com_logical_link_request::CllCreateFlag>,
) -> Result<(bool, bool), Status> {
    use vci_service_interface::create_com_logical_link_request::CllCreateFlag;
    match cll_create_flag {
        None => Ok((false, false)),
        Some(CllCreateFlag::CllCreateFlagBits(bits_msg)) => {
            let mut raw_mode = false;
            let mut checksum_mode = false;
            for &bit_val in &bits_msg.bits {
                let bit = vci_service_interface::CllCreateFlagBit::try_from(bit_val)
                    .unwrap_or(vci_service_interface::CllCreateFlagBit::Unspecified);
                match bit {
                    vci_service_interface::CllCreateFlagBit::CllCreateFlagRawMode => {
                        raw_mode = true;
                    }
                    // ADR-198 Phase 2: now stored -- see this function's own
                    // doc comment.
                    vci_service_interface::CllCreateFlagBit::CllCreateFlagChecksumMode => {
                        checksum_mode = true;
                    }
                    vci_service_interface::CllCreateFlagBit::Unspecified => {
                        return Err(Status::invalid_argument(format!(
                            "cll_create_flag_bits contains an unsupported bit value ({bit_val}) \
                             -- only CLL_CREATE_FLAG_CHECKSUM_MODE/CLL_CREATE_FLAG_RAW_MODE are \
                             defined (ISO 22900-2:2022 Table D.6)"
                        )));
                    }
                }
            }
            Ok((raw_mode, checksum_mode))
        }
        Some(CllCreateFlag::CllCreateFlagRaw(raw)) => {
            let byte0 = raw.first().copied().unwrap_or(0);
            // Every byte from index 1 onward is reserved/Unused (Table
            // D.6) -- validate the WHOLE remainder, not just bytes 1-3, so
            // a caller-supplied array longer than 4 bytes can't smuggle a
            // nonzero byte past this check (Codex review, PR #107 round
            // 1): the prior `.take(3)` cap silently ignored index 4+.
            if byte0 & 0x3F != 0 || raw.iter().skip(1).any(|&b| b != 0) {
                return Err(Status::invalid_argument(
                    "cll_create_flag_raw sets a reserved bit -- ISO 22900-2:2022 Table D.6 \
                     defines only byte 0 bit 6 (ChecksumMode) and byte 0 bit 7 (RawMode); \
                     every other bit is Unused",
                ));
            }
            // ADR-198 Phase 2: byte0 & 0x40 (ChecksumMode) is now stored,
            // not just format-validated -- see this function's own doc
            // comment.
            Ok((byte0 & 0x80 != 0, byte0 & 0x40 != 0))
        }
    }
}

/// Returns the D-PDU `CP_*` shortname for `param_id` if it is one of the
/// five addressing ComParams that `tx_header::j1850_header_bytes` (J1850
/// links) or `tx_header::kwp_header_bytes` (ISO9141/ISO14230 KWP links)
/// narrow with `as u8` (`Some(name)`), or `None` for every other param --
/// used by `rpc_set_com_param`'s `value > 0xFF` range check (PR #53 review
/// round; scope corrected to J1850-family/KWP-family only in a later round,
/// since CAN links also accept `SetComParam` of these five IDs but never
/// forward/truncate them) so the error message names the specific `CP_*`
/// param rather than a raw J2534 config constant. `CP_Node_Address` is
/// `NODE_ADDRESS`'s D-PDU name (`names.rs`'s `map_comparam_name_native`);
/// `CP_5BaudAddressFunc`/`CP_5BaudAddressPhys` are deliberately excluded --
/// they have their own, separate range check immediately above this
/// function's call site.
fn j1850_or_kwp_addressing_byte_param_name(param_id: ComParamId) -> Option<&'static str> {
    match param_id {
        PARAM_PHYS_REQ_FORMAT_PRIORITY => Some("CP_PhysReqFormatPriorityType"),
        PARAM_PHYS_REQ_TARGET_ADDR => Some("CP_PhysReqTargetAddr"),
        PARAM_FUNC_REQ_FORMAT_PRIORITY => Some("CP_FuncReqFormatPriorityType"),
        PARAM_FUNC_REQ_TARGET_ADDR => Some("CP_FuncReqTargetAddr"),
        ComParamId(j2534_0404::NODE_ADDRESS) => Some("CP_Node_Address"),
        _ => None,
    }
}

/// Bundled connect-time parameters for
/// [`J2534Service::connect_new_physical_channel`] (clippy `too_many_arguments`
/// fix, ADR-157) -- grouped since every field is always taken together from
/// one `LogicalLinkState` snapshot at the single call site.
struct NewPhysicalChannelParams<'a> {
    device_id: DeviceId,
    /// Raw hardware protocol id (Plane A, ADR-157) -- may be a `_PS`
    /// variant; `connect_new_physical_channel` normalizes internally for
    /// every Plane B decision it makes.
    j2534_proto_id: u32,
    baud_rate: u32,
    connect_flags: u32,
    working_snapshot: &'a ComParamSet,
    pin_select: Option<u32>,
    /// SAE J2534-2 clause 21 CAN FD data phase rate (ADR-158): `Some(rate)`
    /// for an FD_CAN_PS link (`rpc_connect_com_logical_link`'s snapshot
    /// block computes `rate` from `CP_CANFDBaudrate` if nonzero, else
    /// `CP_Baudrate`), `None` for every other connect.
    /// `connect_new_physical_channel` issues
    /// `SET_CONFIG(CONFIG_FD_CAN_DATA_PHASE_RATE, rate)` before
    /// `CONFIG_J1962_PINS` when `Some` (clause 21.3.2.5.1's mandatory
    /// ordering), and skips this step entirely when `None`.
    fd_data_phase_rate: Option<u32>,
    /// SAE J2534-2 clause 8 Mixed Format Frames on a CAN Network
    /// (ADR-160/Phase 3c Stage 3c; `Option<u32>` shape added by ADR-217):
    /// `None` iff this new physical channel is not ISO15765-family, is
    /// FD-substituted, or is not connecting under either native-mixed
    /// sub-mode (`CanChannelMode::NativeMixed` /
    /// `CanChannelMode::NativeMixedAllFrames`, computed once by
    /// `rpc_connect_com_logical_link`, before `device_id` is acquired --
    /// see that call site's own doc comment); otherwise `Some(value)` with
    /// `value` being `CAN_MIXED_FORMAT_ON` or `CAN_MIXED_FORMAT_ALL_FRAMES`
    /// depending on which sub-mode resolved. `connect_new_physical_channel`
    /// issues `SET_CONFIG(CONFIG_CAN_MIXED_FORMAT, value)` once, right after
    /// the FD data-phase-rate step, when `Some`; skipped entirely when
    /// `None`. Never re-issued for a CLL joining an already-open shared
    /// channel -- this function is never called for that case.
    native_mixed_format: Option<u32>,
    /// SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15; revised by
    /// ADR-178): `Some(rate)` for a physical channel opened with one of the
    /// 32 native `PROTOCOL_ANALOG_IN_x` ids (`rpc_connect_com_logical_link`'s
    /// snapshot block reads this CLL's staged `PARAM_ANALOG_SAMPLE_RATE`
    /// Working ComParam and validates it nonzero right there), `None` for
    /// every other connect. `connect_new_physical_channel` issues
    /// `SET_CONFIG(CONFIG_SAMPLE_RATE, rate)` when `Some`, mirroring
    /// `fd_data_phase_rate`'s own connect-time SET_CONFIG shape (ADR-158) --
    /// a device rejection fails the whole connect the same way.
    analog_sample_rate: Option<u32>,
}

/// SAE J2534-2 clause 8's `CAN_MIXED_FORMAT` `SET_CONFIG`/`GET_CONFIG`
/// payload values (ADR-160): the native header defines `CONFIG_CAN_MIXED_
/// FORMAT` (`0x8000`, the parameter ID) but no value constants, so these are
/// plain local `u32`s rather than a bindgen/header addition.
#[allow(dead_code)] // no GET_CONFIG/OFF-toggle caller yet -- this stage only ever sets ON.
const CAN_MIXED_FORMAT_OFF: u32 = 0;
const CAN_MIXED_FORMAT_ON: u32 = 1;
// Used by `CanChannelMode::NativeMixedAllFrames`'s connect-time SET_CONFIG
// (ADR-217).
const CAN_MIXED_FORMAT_ALL_FRAMES: u32 = 2;

/// SAE J2534-2 clause 21's CAN FD trigger (ADR-158): `true` iff `params`
/// signals CAN FD mode -- `CP_CANFDTxMaxDataLength > 8` (`TX_DL == 8` alone
/// is Classic CAN's own max payload and therefore ambiguous on its own) or a
/// nonzero `CP_CANFDBaudrate` (`0` is itself a valid "use `CP_Baudrate`"
/// default, so a nonzero value is decisive on its own regardless of
/// `TX_DL`).
///
/// Shared by `J2534Service::apply_fd_mode`'s connect-time substitution and
/// `rpc_primitive::rpc_start_com_primitive`'s `CoptUpdateparam` guard
/// (ADR-158 correction, Codex review PR #30 round 3) so the two call sites
/// -- one deciding whether to substitute the physical channel at connect
/// time, the other deciding whether a live promotion would contradict an
/// already-connected channel's FD-ness -- can never drift apart on what
/// counts as "FD requested."
pub(super) fn fd_mode_staged(params: &ComParamSet) -> bool {
    let tx_dl = params
        .unum32
        .get(&PARAM_CANFD_TX_MAX_DATA_LENGTH)
        .copied()
        .unwrap_or(0);
    let canfd_baudrate = params
        .unum32
        .get(&PARAM_CANFD_BAUDRATE)
        .copied()
        .unwrap_or(0);
    tx_dl > 8 || canfd_baudrate != 0
}

/// SAE J2534-1 TX message size range for an FD-connected link (ADR-158
/// correction, Codex review PR #30 round 4): `4` (CAN ID header, ADR-050) +
/// `0..=effective_tx_dl` data bytes, where `effective_tx_dl` is the
/// currently-staged `CP_CANFDTxMaxDataLength` -- ISO 22900-2's defining
/// semantic for this ComParam is the tester's declared max TX length
/// (ISO 15765-2's TX_DL), so the cap tracks live `CoptUpdateparam`
/// promotions within FD mode, not a fixed 64. `TX_DL` staged as `0` or `8`
/// (Classic CAN's own max, ambiguous alone -- see [`fd_mode_staged`]) still
/// reaches an FD-connected link when only `CP_CANFDBaudrate` is nonzero;
/// ISO 22900-2's own fallback for an unset `TX_DL` is `8`, hence
/// `.max(8)` rather than trusting the raw staged value.
///
/// Deliberately separate from [`ChannelProtocol::tx_message_size_range`]
/// (`protocol.rs`) rather than a parameter on it: that function is the pure
/// SAE J2534-1 per-protocol table, with a lockstep unit test asserting every
/// row -- the FD bound is link *state* (a live ComParam), not a protocol
/// constant, and would force every non-FD caller to thread an unused
/// parameter through it.
pub(super) fn fd_can_tx_message_size_range(params: &ComParamSet) -> RangeInclusive<usize> {
    let effective_tx_dl = effective_fd_tx_dl(params);
    4..=(4 + effective_tx_dl)
}

/// The effective staged `CP_CANFDTxMaxDataLength` (ADR-169): the raw staged
/// value, or `8` (Classic CAN's own max) when unset/staged as `0` -- the same
/// fallback [`fd_can_tx_message_size_range`] above has always applied,
/// factored out so a second caller (`rpc_primitive.rs`'s functional-SF check
/// against a native `FD_ISO15765_PS` link) shares one definition of "the
/// effective staged FD frame size" instead of duplicating the `.max(8)` rule.
pub(super) fn effective_fd_tx_dl(params: &ComParamSet) -> usize {
    params
        .unum32
        .get(&PARAM_CANFD_TX_MAX_DATA_LENGTH)
        .copied()
        .unwrap_or(0)
        .max(8) as usize
}

/// ADR-162 Decision 2: `J2534Service::promote_unique_resp_id_table` rejected
/// a candidate `UniqueRespIdTable` because it would introduce a native-mixed-
/// mode `CP_CanRespUUDTId`/`CP_CanRespUSDTId` match-key collision on the
/// shared physical channel -- nothing was written (old table and filters
/// stay installed). Carries the colliding `CanAddress` (private to this
/// module, like `CanAddress` itself -- callers outside `rpc_link.rs`, e.g.
/// `events::handle_update_param`, only need to know the promotion was
/// rejected, matched via `Err(_)`, not the address itself).
#[derive(Debug)]
pub(super) struct NativeMixedTableCollision(CanAddress);

impl J2534Service {
    pub(super) async fn rpc_get_resource_status(
        &self,
        request: Request<vci_service_interface::GetResourceStatusRequest>,
    ) -> Result<Response<vci_service_interface::ResourceStatusResponse>, Status> {
        let request = request.into_inner();
        let mut resource_status_data = Vec::with_capacity(request.resources.len());

        // Held for the entire loop (device_id is the outermost lock; the only
        // nested acquisition below is `logical_links`, the established
        // device_id -> logical_links order -- see `lock_device_for`'s docs and
        // `rpc_module_disconnect`). Freezes the open-module identity so the
        // per-entry gate below can never match a stale module against another
        // module's live links, and makes a multi-entry response a consistent
        // snapshot of one device state.
        let device_slot = self.device_id.lock().await;
        let open_module_handle = device_slot.map(|(h, _)| h);

        for entry in request.resources {
            let module_handle = entry
                .module_handle
                .ok_or_else(|| Status::invalid_argument("module_handle is required"))?;
            Self::require_module_handle(Some(module_handle), self.modules.len())?;

            let resource = entry
                .resource
                .ok_or_else(|| Status::invalid_argument("resource is required"))?;

            // ADR-069: `resource_id` may be an opaque resources-table ID
            // (e.g. from `GetResourceIds`), not a raw `ChannelProtocol`
            // value -- resolve it through the table first, falling back to
            // `ChannelProtocol::from_raw` for anything not in the table (a
            // raw/extended `ChannelProtocol` value, unaffected by ADR-069).
            // A `ResourceName` goes through the table first too
            // (`find_table_rows_by_name`), which also covers table-only
            // names (e.g. `"ISO_OBD_on_K_Line"`) the legacy
            // `map_object_type_name` never recognized; falls back to
            // `map_protocol_name` when the name matches no table row at
            // all. `query_resource_id` is `Some` only for a `ResourceId`
            // query, so the response always echoes exactly the ID the
            // caller passed in that case (see below). `table_rows` is
            // non-empty only for a direct table-name match, and is kept
            // separately from `resolved_protocols` (the flattened,
            // deduplicated predicate set) so the echo below can report a
            // *specific* row's own resource ID rather than re-deriving one
            // from its `ChannelProtocol` alone -- necessary because a
            // uniquely-named row can share its `ChannelProtocol` with an
            // alias row (e.g. `"ISO_OBD_on_K_Line"` names resource 0x0213,
            // which shares a `ChannelProtocol` with 0x0212).
            // Each resolved candidate is `(ChannelProtocol, hw_protocol_override)`:
            // for the four `SAE_J2610_on_SAE_J2610_SCI` rows (0x021E..0x0221),
            // which share one `ChannelProtocol` but override distinct hardware
            // protocol IDs, the override is what actually distinguishes them at
            // `ConnectComLogicalLink` time (it becomes the link's own
            // `hw_protocol_id`) -- so it must also gate the active-link match
            // below, or querying one specific row (e.g. 0x021E) would falsely
            // report active whenever *any* other 0x0160 configuration (e.g.
            // 0x0221) is connected. `None` means the row (or a legacy
            // `map_protocol_name` match, which never carries an override) has
            // no fixed hardware override and matches on `protocol` alone, same
            // as before this check existed (e.g. the J1850 auto-detect rows).
            let (query_resource_id, mut resolved, table_rows): (
                Option<u32>,
                Vec<ResolvedProtocolCandidate>,
                Vec<&'static resources::ResourceDef>,
            ) = match resource {
                vci_service_interface::module_and_resource_id::Resource::ResourceId(id) => {
                    let (protocol, hw_override) = match resources::find_by_resource_id(id) {
                        Some(row) => (row.protocol, row.hw_protocol_override),
                        // ADR-157 Correction (design-advisor, PR #28): `id`
                        // can numerically equal a raw `_PS` hardware
                        // protocol id -- normalize via
                        // `resources::base_protocol_id` before wrapping, the
                        // query-side sibling of `names.rs::
                        // parse_protocol_id_from_resource`'s own tail
                        // normalization. A directly-named `_PS` link's
                        // stored `protocol` is now always the base identity,
                        // so an un-normalized `_PS` query id could never
                        // match it in the `active_candidate` comparison
                        // below, falsely reporting the resource as idle.
                        // `query_resource_id` (echoed back verbatim in the
                        // response) is captured separately, above, and stays
                        // the caller's literal `id` regardless.
                        // ADR-164/ADR-168 correction (Codex review round 3):
                        // an SW/FT `_PS` id is the ONE `_PS` family whose
                        // connected link stores this exact qualified id as
                        // its own raw `hw_protocol_id`, not the base
                        // identity the comment above assumes every `_PS`
                        // family normalizes to (`matches_status_hw_id`'s own
                        // ADR-164/ADR-168 "Bug 1" guards, below, depend on
                        // this). Collapsing an un-tabled SW/FT query id down
                        // to a bare base override (as the general case does)
                        // would make `status_hw_id`/`hardware_native_hw_id`
                        // (computed from this candidate further down) equal
                        // to the base id, which those same guards then
                        // refuse to match against a connected SW/FT link's
                        // own raw id -- so a raw-id query for the SW/FT
                        // resource specifically would falsely report it
                        // idle, while a plain base-family link could
                        // satisfy it instead. Preserve `id` as the override
                        // here so this candidate carries the same identity
                        // shape a real SW/FT resource-table row's
                        // `hw_protocol_override` does.
                        None if resources::is_sw_family_protocol_id(id)
                            || resources::is_ft_family_protocol_id(id) =>
                        {
                            (
                                ChannelProtocol::from_raw(resources::base_protocol_id(id)),
                                Some(id),
                            )
                        }
                        None => (
                            ChannelProtocol::from_raw(resources::base_protocol_id(id)),
                            None,
                        ),
                    };
                    (Some(id), vec![(protocol, hw_override)], Vec::new())
                }
                vci_service_interface::module_and_resource_id::Resource::ResourceName(name) => {
                    let rows = Self::find_table_rows_by_name(&name);
                    if rows.is_empty() {
                        let protocols: Vec<ResolvedProtocolCandidate> =
                            Self::map_protocol_name(&name)
                                .into_iter()
                                .map(|p| (p, None))
                                .collect();
                        if protocols.is_empty() {
                            // A3-1/A3-2: a name that resolves through neither
                            // the resources table nor the legacy protocol-name
                            // aliases previously fell through to a silent
                            // resource_id: 0 / resource_status: 0 echo with no
                            // error -- indistinguishable from a real query
                            // that happens to report an idle, unlocked
                            // resource. §9.4.8.5 Table 16 lists a resource id
                            // this call can't recognize among the conditions
                            // that return `PDU_ERR_INVALID_PARAMETERS`, and
                            // rejecting an unrecognized name matches this
                            // service's own ADR-078 philosophy elsewhere
                            // (e.g. `parse_protocol_id_from_resource`, used by
                            // `CreateComLogicalLink`).
                            return Err(state_guard_status(
                                Code::InvalidArgument,
                                format!(
                                    "PDU_ERR_INVALID_PARAMETERS: unrecognized resource name {name:?}"
                                ),
                                PduError::PduErrInvalidParameters,
                                None,
                            ));
                        }
                        (None, protocols, Vec::new())
                    } else {
                        let protocols: Vec<ResolvedProtocolCandidate> = rows
                            .iter()
                            .map(|row| (row.protocol, row.hw_protocol_override))
                            .collect();
                        (None, protocols, rows)
                    }
                }
            };

            // ADR-164/Phase 4 correction: a stable sort placing every
            // `hw_override: Some(_)` candidate before every `hw_override:
            // None` candidate. Needed now that an SW resource row's
            // `hw_protocol_override` (a genuine `_PS`-qualified id, unlike
            // every other `Some(_)` override in the table, which is a native
            // base hardware id) can share one `ChannelProtocol` with a
            // `None`-override dual-wire sibling: below, a `None` candidate's
            // `is_none_or` match is a wildcard ("any hardware realization of
            // this protocol") that is only correct once no more-specific
            // `Some(_)` sibling candidate for the same query has already
            // claimed the live link -- `.find()` below returns the first
            // match, so `Some(_)` candidates must be checked first. `sort_by`
            // (stable) preserves each group's original table-order relative
            // sequence, so this is a no-op for every pre-existing case where
            // a query name's candidates never mix `None` and `Some(_)`
            // overrides (e.g. the four `SAE_J2610_on_SAE_J2610_SCI` rows,
            // all `Some(_)`, or a single-candidate legacy `map_protocol_name`
            // match).
            resolved.sort_by_key(|&(_, hw_override)| hw_override.is_none());

            // Picks whichever resolved candidate has an active link, if any
            // (there can be more than one candidate for an ambiguous name
            // like `"SAE_J2610_SCI"`); this is also the candidate the
            // response's `resource_id` echoes for a `ResourceName` query
            // (below), so an active-link match is preferred there too. A
            // candidate with `hw_override: Some(_)` only matches a link whose
            // own `hw_protocol_id` equals it -- see the comment above.
            // A resource can only be active for the module that is actually
            // open right now (single-open-device model, ADR-107) -- querying
            // an unopened (but in-range/configured) module must not scan
            // `logical_links`, which belongs entirely to whichever module IS
            // open, regardless of which module this iteration is asking
            // about.
            // ADR-127 (finding A2-3): the "in use" and lock-status bits need
            // a different resource-identity test than `active_candidate`'s
            // `ChannelProtocol` match. `hw_protocol_id` equality (the same
            // comparison `same_physical_resource`/`find_physical_lock_holder`
            // use for `LockResource`/`UnlockResource`, ADR-123) is required
            // so a software-ISO-TP raw-CAN CLL correctly shows up when its
            // ISO15765 sibling's resource is queried, and vice versa
            // (ADR-046) -- two `ChannelProtocol` values sharing one physical
            // CAN channel. Crucially, this match is scoped to a SINGLE
            // candidate (`status_candidate`, below) -- the very one that
            // ends up echoed as `resource_id` further down this function --
            // not every row of an ambiguous `ResourceName` match (e.g.
            // `"SAE_J2610_SCI"`'s four rows). `resource_status` must describe
            // the same resource the response's own `resource_id` names, or a
            // caller sees an internally inconsistent pair (e.g. `resource_id`
            // for an idle row alongside `resource_status` bit 0 borrowed from
            // a *different*, in-use sibling row of the same ambiguous name).
            // Both new flags are computed under the same `links` lock guard
            // as `active_candidate` (no second acquisition), gated by the
            // same open-module check (ADR-107).
            let (active_candidate, in_use, held_lock_mask) = if open_module_handle
                == Some(module_handle.module_handle)
            {
                let links = self.logical_links.lock().await;
                let candidate = resolved.iter().copied().find(|&(p, hw_override)| {
                    links.values().any(|link| {
                        link.protocol == p
                                && link.connected
                                // ADR-157: normalize to the link's base hw
                                // protocol id before comparing against a
                                // resource row's `hw_protocol_override` --
                                // for a pin-selected SCI link,
                                // `link.hw_protocol_id` holds the
                                // consolidated `PROTOCOL_J2610_PS` id, not
                                // the exact base SCI variant a table row's
                                // override names.
                                //
                                // ADR-164/Phase 4 correction: also match the
                                // link's RAW (un-normalized) `hw_protocol_id`
                                // -- an SW row's own override IS the raw
                                // `_PS` id a connected SW link's
                                // `hw_protocol_id` holds directly (unlike
                                // SCI's override, a base id that only
                                // survives in `base_hw_protocol_id()`), so
                                // `base_hw_protocol_id()` alone (which
                                // normalizes SW back down to plain CAN/
                                // ISO15765) would never match it. Purely
                                // additive: every pre-existing match via
                                // `base_hw_protocol_id()` alone still holds.
                                && hw_override.is_none_or(|hw| {
                                    hw == link.hw_protocol_id || hw == link.base_hw_protocol_id()
                                })
                    })
                });

                // Mirrors the `resource_id` echo's own row-selection logic
                // exactly (see `let resource_id = query_resource_id...`
                // below): prefer the `table_rows` entry matching `candidate`,
                // else the first `table_rows` entry; when there is no
                // `table_rows` at all (legacy `map_protocol_name` path),
                // fall back to `candidate`, else the first resolved
                // candidate. Kept as a second, deliberately duplicated
                // computation rather than a shared helper so the
                // already-tested echo block below stays untouched.
                let status_candidate: Option<ResolvedProtocolCandidate> = if !table_rows.is_empty()
                {
                    candidate
                        .and_then(|(p, hw_override)| {
                            table_rows.iter().find(|row| {
                                row.protocol == p && row.hw_protocol_override == hw_override
                            })
                        })
                        .map(|row| (row.protocol, row.hw_protocol_override))
                        .or_else(|| {
                            table_rows
                                .first()
                                .map(|row| (row.protocol, row.hw_protocol_override))
                        })
                } else {
                    candidate.or_else(|| resolved.first().copied())
                };

                let status_hw_id = status_candidate.map(|(p, hw_override)| {
                    hw_override.unwrap_or_else(|| self.can_channel_mode.hw_protocol_id(p))
                });
                // Codex review, PR #28: `status_hw_id` above substitutes
                // the module-wide `can_channel_mode`'s hardware mapping
                // (raw `CAN` for an ISO15765-family candidate in
                // `SoftwareIsoTp` mode) -- but Pin Selection is a
                // per-link override of that module-wide mode (ADR-157's
                // "`_PS` links are hardware-ISO-TP only" residual: a
                // pin-selected `ISO15765_PS` link's `software_isotp` is
                // always `false`, so it stays on the natural ISO15765
                // hardware identity regardless of the module's software-
                // ISO-TP setting). `status_hw_id` alone therefore misses
                // exactly that link when the two ids diverge -- computed
                // here without the mode substitution, so a query for the
                // ISO15765 resource in software-ISO-TP mode still sees a
                // hardware-ISO-TP `_PS` link as occupying it.
                let hardware_native_hw_id = status_candidate
                    .map(|(p, hw_override)| hw_override.unwrap_or_else(|| p.j2534_protocol_id()));
                // Codex review, PR #28: `PROTOCOL_J2610_PS` collapses all
                // four native SAE J2610 SCI hardware ids onto one `_PS` id
                // (`resources::ps_protocol_id`'s Table 1 consolidation
                // note); `resources::base_protocol_id` -- what produced
                // `hardware_native_hw_id` above via the query-side fallback
                // a few lines up -- picks a single representative
                // (`SCI_A_ENGINE`) for that collapse, by design (every
                // OTHER Plane B consumer of the free function gates
                // identically across all four variants, so the collapse
                // costs them nothing). A raw `PROTOCOL_J2610_PS` resource
                // query is different: it asks about the consolidated `_PS`
                // id itself, unqualified by any specific variant, so it
                // must match a connected link using ANY of the four, not
                // just the one representative -- otherwise an
                // `SCI_B_TRANS`-based pin-selected link (whose
                // `base_hw_protocol_id()` correctly preserves the exact
                // variant, unlike the free function) is falsely reported
                // idle. `query_resource_id` is the caller's literal,
                // un-normalized `id` -- comparing it (not
                // `hardware_native_hw_id`) against the raw `_PS` constant is
                // what distinguishes "queried the consolidated _PS id
                // directly" from "queried SCI_A_ENGINE's own raw hardware id
                // directly" (both would otherwise produce the same
                // `hardware_native_hw_id`).
                let queried_raw_j2610_ps = query_resource_id == Some(j2534_0404::PROTOCOL_J2610_PS);
                // ADR-156 Decision 3 addendum/Phase 2b: a literal `_CHx`
                // query id -- in ANY of the seven in-scope families, not just
                // J2610/SCI -- must be scoped to its exact index (Codex
                // review, PR #28/edge-case-hunter finding 1). Generalized
                // across all seven families, this becomes a precondition
                // (AND-gated) on the whole match, not an additional OR-branch:
                // a plain base-id match on `status_hw_id`/
                // `hardware_native_hw_id` alone is not sufficient for a
                // literal `_CHx` query, since `base_protocol_id`'s Phase 2b
                // `_CHx` funnel extension normalizes every index of a family
                // onto the same base id -- without the index precondition, a
                // query for one index would falsely match a connected link at
                // a different index of the same family.
                let queried_chx_index = query_resource_id
                    .and_then(resources::chx_base_protocol_id)
                    .map(|(_, index)| index);
                // Kept narrow (SCI only) for the existing variant-broadening
                // OR-branch below: a raw `_CHx` query id inside the
                // `PROTOCOL_J2610_CHx` block decomposes via
                // `resources::chx_base_protocol_id` to ONE representative SCI
                // variant (`SCI_A_ENGINE`) plus a channel index -- the query
                // id itself never distinguishes which of the four SCI natives
                // the caller means, since `chx_protocol_id` already collapses
                // all four onto the same block/id per index -- so it must
                // match a connected link on ANY SCI variant AT THAT SAME
                // INDEX (the `queried_chx_index` precondition above still
                // applies: a literal `_CHx` query matches only its exact
                // index, never a different index of the same family).
                let queried_j2610_chx = query_resource_id
                    .and_then(resources::chx_base_protocol_id)
                    .is_some_and(|(base, _)| base == j2534_0404::SCI_A_ENGINE);
                // ADR-164/Phase 4 correction: takes both the link's raw
                // `hw_protocol_id` and its normalized `base_hw_protocol_id()`
                // (previously only the latter) -- an SW link's own connect id
                // IS the qualified `_PS` id a table row's `hw_protocol_override`
                // names directly (unlike SCI's override, a base id that
                // `base_hw_protocol_id()` alone already preserves), so
                // `hw_id` is what actually matches `status_hw_id`/
                // `hardware_native_hw_id` for a query naming the SW resource
                // specifically; `base` still carries every pre-existing
                // match (including the SCI/FD cases, whose raw id would NOT
                // match a query candidate's base-level override). Checked as
                // two independent candidates, both OR'd in below, rather than
                // choosing one -- purely additive over the single-`base`
                // version.
                //
                // ADR-164/Bug 1 fix (edge-case-hunter audit): the `base`
                // candidate above is dropped entirely when `hw_id` itself is
                // an SW `_PS` id (`resources::is_sw_protocol_id`). Every
                // other `_PS`/`_CHx` family's base-normalized identity IS the
                // same physical bus as the qualified variant (that's the
                // whole point of `base_hw_protocol_id()`'s Plane B funnel),
                // but SWCAN/SW-ISO15765 are the one family where
                // `base_protocol_id` collapses onto a sibling
                // (`CAN`/`ISO15765`) that is a genuinely different physical
                // bus (own pin, own `bus_type_id`, ADR-164's Context/
                // Consequences sections) -- so matching on `base` here would
                // misattribute an SW link's "in use"/lock status onto its
                // plain dual-wire sibling's `GetResourceStatus` query. This
                // mirrors `same_physical_resource` (`service.rs`, the real
                // contention check `LockResource`/`UnlockResource` use),
                // which compares raw `hw_protocol_id` only and never
                // normalizes to base at all. `hw_id` itself (the un-dropped
                // candidate) still lets an SW link satisfy a query naming the
                // SW resource specifically, so this is a narrowing, not a
                // full removal, of the SW link's candidate set.
                let matches_status_hw_id = |hw_id: u32, base: u32, channel_index: Option<u32>| {
                    if let Some(idx) = queried_chx_index
                        && channel_index != Some(idx)
                    {
                        return false;
                    }
                    [hw_id, base].into_iter().any(|candidate_id| {
                        if candidate_id != hw_id
                            && (resources::is_sw_family_protocol_id(hw_id)
                                || resources::is_ft_family_protocol_id(hw_id))
                        {
                            return false;
                        }
                        status_hw_id == Some(candidate_id)
                            || hardware_native_hw_id == Some(candidate_id)
                            || (queried_raw_j2610_ps
                                && resources::is_sci_hw_protocol_id(candidate_id))
                            || (queried_j2610_chx && resources::is_sci_hw_protocol_id(candidate_id))
                    })
                };

                // Bit 0 ("in use"): true from `CreateComLogicalLink` onward
                // (§9.4.9.2 c)), independent of `connected` -- a created but
                // unconnected CLL still occupies the resource.
                // ADR-157: normalize the link side to its base protocol id
                // before comparing -- `hw_id` (from the static resource
                // table) is already a base id, so a `_PS` link must be
                // compared on the same footing to be seen as occupying its
                // base protocol's resource (a consistency requirement:
                // `resource_status` must describe the same resource
                // `resource_id` names). Matches `status_hw_id` (the
                // mode-adjusted identity), `hardware_native_hw_id` (the
                // unadjusted one), or -- for a raw `PROTOCOL_J2610_PS` query
                // specifically -- any of the four SCI variants; see the
                // comments above.
                let in_use = links.values().any(|link| {
                    matches_status_hw_id(
                        link.hw_protocol_id,
                        link.base_hw_protocol_id(),
                        link.channel_index,
                    )
                });
                // Bits 2/3: OR every matching link's `held_lock_mask`
                // (regardless of `connected`) -- Table D.1's lock bits
                // describe lock existence on the resource, not who holds it
                // or whether that CLL happens to be connected.
                let held_lock_mask = links
                    .values()
                    // ADR-157: same match as `in_use` above.
                    .filter(|link| {
                        matches_status_hw_id(
                            link.hw_protocol_id,
                            link.base_hw_protocol_id(),
                            link.channel_index,
                        )
                    })
                    .fold(0u32, |mask, link| mask | link.held_lock_mask);

                (candidate, in_use, held_lock_mask)
            } else {
                (None, false, 0u32)
            };
            let mut resource_status = if in_use { 0x01 } else { 0 };
            if held_lock_mask & LOCK_PHYSICAL_TX_QUEUE != 0 {
                resource_status |= 0x04;
            }
            if held_lock_mask & LOCK_PHYSICAL_COM_PARAMS != 0 {
                resource_status |= 0x08;
            }

            // A `ResourceId` query always echoes exactly the ID the caller
            // passed.
            //
            // A `ResourceName` query that matched the table directly (one
            // row, or several for `"SAE_J2610_SCI"`/
            // `"SAE_J2610_on_SAE_J2610_SCI"`) echoes that specific row's own
            // resource ID -- preferring whichever matched row has an active
            // link (`active_candidate`, matched by `protocol` *and*
            // `hw_protocol_override`), else the first row (already in table
            // order). A name resolved only via the legacy `map_protocol_name`
            // (no direct table-name match) echoes the resources-table ID for
            // that protocol via `find_resource_id_for_protocol`'s table-order
            // tie-break for a protocol shared by several rows (e.g.
            // `"ISO15765"` -> `ChannelProtocol::ISO15765` -> resource
            // `0x0206`), falling back to the raw `ChannelProtocol` value when
            // the protocol has no table row at all (a legacy-only extended
            // protocol, matching the pre-ADR-069 echo unchanged). `0` when
            // the name resolved to nothing whatsoever.
            let resource_id = query_resource_id.unwrap_or_else(|| {
                if let Some(row) = active_candidate
                    .and_then(|(p, hw_override)| {
                        table_rows.iter().find(|row| {
                            row.protocol == p && row.hw_protocol_override == hw_override
                        })
                    })
                    .or_else(|| table_rows.first())
                {
                    return row.resource_id;
                }
                active_candidate
                    .or_else(|| resolved.first().copied())
                    .map(|(p, _hw_override)| {
                        resources::find_resource_id_for_protocol(p).unwrap_or(p.value())
                    })
                    .unwrap_or(0)
            });

            resource_status_data.push(vci_service_interface::ResourceStatusData {
                module_handle: Some(module_handle),
                resource_id,
                resource_status,
            });
        }
        drop(device_slot);

        Ok(Response::new(
            vci_service_interface::ResourceStatusResponse {
                resource_status: Some(vci_service_interface::ResourceStatusItem {
                    resource_status_data,
                }),
            },
        ))
    }

    pub(super) async fn rpc_get_resource_ids(
        &self,
        request: Request<vci_service_interface::GetResourceIdsRequest>,
    ) -> Result<Response<vci_service_interface::ResourceIdsResponse>, Status> {
        let request = request.into_inner();
        let module_handle = request
            .module_handle
            .ok_or_else(|| Status::invalid_argument("module_handle is required"))?;

        let requested_handle = module_handle.module_handle;
        let module_count = self.modules.len() as u32;
        let in_range = requested_handle != 0 && requested_handle <= module_count;
        if !in_range && requested_handle != PDU_HANDLE_UNDEF {
            return Err(Status::invalid_argument(format!(
                "unsupported module_handle {requested_handle}; expected a value in \
                 1..={module_count} or {PDU_HANDLE_UNDEF}",
            )));
        }

        let resource_data = request
            .resource_data
            .ok_or_else(|| Status::invalid_argument("resource_data is required"))?;

        let resource_ids = Self::resolve_resource_ids_from_data(&resource_data)?;
        let response_module_handle = if requested_handle == PDU_HANDLE_UNDEF {
            DEFAULT_MODULE_HANDLE
        } else {
            requested_handle
        };

        Ok(Response::new(vci_service_interface::ResourceIdsResponse {
            resource_id_list: Some(vci_service_interface::ResourceIdItem {
                resource_id_data_array: vec![vci_service_interface::ResourceIdItemData {
                    module_handle: Some(vci_service_interface::ModuleHandle {
                        module_handle: response_module_handle,
                    }),
                    resource_id_array: resource_ids,
                }],
            }),
        }))
    }

    /// ADR-106: a static resource-table (pin/controller conflict) query per
    /// ISO 22900-2 §9.4.26, independent of any live connection state and
    /// callable before any `ComLogicalLink` exists -- see
    /// `resources::rows_conflict` for the conflict predicate itself. This
    /// replaces the earlier live-CLL-based scan entirely: reporting a
    /// currently-connected same-protocol CLL as a "conflict" was wrong,
    /// since ISO 22900-2 explicitly allows several CLLs to share one
    /// physical channel (same protocol/baud) without that being a resource
    /// conflict; `GetResourceStatus` is the correct RPC for live in-use/lock
    /// state.
    pub(super) async fn rpc_get_conflicting_resources(
        &self,
        request: Request<vci_service_interface::GetConflictingResourcesRequest>,
    ) -> Result<Response<vci_service_interface::ConflictingResourcesResponse>, Status> {
        let request = request.into_inner();

        // §9.4.26.2 a) "validate all input parameters": `resource` (oneof)
        // entirely unset is the NULL-pointer-equivalent case. A `resource_id`
        // with no matching table row (a legacy/raw `ChannelProtocol` value,
        // ADR-069) or a `resource_name` matching no table row both resolve
        // to an empty queried set rather than an error -- no pin/bus
        // metadata exists for either to compute conflicts from, and
        // reintroducing a protocol-only fallback match here would bring
        // back the "protocol sharing != conflict" bug this ADR fixes.
        let queried: Vec<&'static resources::ResourceDef> = match request.resource {
            Some(
                vci_service_interface::get_conflicting_resources_request::Resource::ResourceId(id),
            ) => resources::find_by_resource_id(id).into_iter().collect(),
            Some(
                vci_service_interface::get_conflicting_resources_request::Resource::ResourceName(
                    name,
                ),
            ) => Self::find_table_rows_by_name(&name),
            None => {
                return Err(state_guard_status(
                    Code::InvalidArgument,
                    "PDU_ERR_INVALID_PARAMETERS: resource (resource_id or resource_name) is required",
                    PduError::PduErrInvalidParameters,
                    None,
                ));
            }
        };

        // §9.4.26.2 a): `pInputModuleList` NULL-equivalent check.
        let module_list = request.input_module_list.ok_or_else(|| {
            state_guard_status(
                Code::InvalidArgument,
                "PDU_ERR_INVALID_PARAMETERS: input_module_list is required",
                PduError::PduErrInvalidParameters,
                None,
            )
        })?;

        // Every named module must be within the configured range; fail
        // closed on the first bad entry rather than skipping it (ADR-107).
        for module in &module_list.module_data {
            Self::require_module_handle(module.module_handle, self.modules.len())?;
        }

        // A present-but-empty module list is a valid call with nothing to
        // check against.
        if module_list.module_data.is_empty() {
            return Ok(Response::new(
                vci_service_interface::ConflictingResourcesResponse {
                    conflict_list: Some(vci_service_interface::ResourceConflictItem {
                        resource_conflict_data: Vec::new(),
                    }),
                },
            ));
        }

        // Union the conflicts of every queried row, deduplicated by
        // resource_id -- iterating the table once (rather than the queried
        // set, then the table) both dedups automatically and emits in
        // RESOURCE_TABLE order. A queried row can legitimately appear here
        // as a conflict of another queried row (e.g. two SCI configurations
        // sharing a pin) when the query itself resolved to multiple rows --
        // that is correct, not filtered out.
        let conflicting_resource_ids: Vec<u32> = resources::resource_table()
            .iter()
            .filter(|candidate| {
                queried
                    .iter()
                    .any(|row| resources::rows_conflict(row, candidate))
            })
            .map(|row| row.resource_id)
            .collect();

        // `resources::resource_table()` is a single static hardware-pin/
        // controller table, not itself partitioned per module (ADR-106) --
        // so every module in `module_list.module_data` shares the exact same
        // computed conflict set, per row. Per ISO 22900-2 §9.4.26.2 b) and
        // the `PDU_RSC_CONFLICT_DATA.hMod` field (§11.1.4.9: the handle of
        // the module in which the conflict exists), each output row must be
        // tagged with the handle of the module it applies to, not a
        // hardcoded default -- so emit the cross product of the queried
        // modules (in the order given) x the conflicting resource IDs (in
        // table order within each module's group), using each module's own
        // (already-validated) handle rather than `DEFAULT_MODULE_HANDLE`.
        // Not deduplicated by handle: if `input_module_list.module_data`
        // names the same `module_handle` twice, this cross product emits the
        // conflict rows for that handle twice too. Intentional pass-through
        // of whatever the caller queried (the input list itself isn't
        // deduplicated either), not a bug.
        let conflict_data = module_list
            .module_data
            .iter()
            .flat_map(|module| {
                conflicting_resource_ids.iter().map(move |&resource_id| {
                    vci_service_interface::ResourceConflictData {
                        module_handle: module.module_handle,
                        resource_id,
                    }
                })
            })
            .collect();

        Ok(Response::new(
            vci_service_interface::ConflictingResourcesResponse {
                conflict_list: Some(vci_service_interface::ResourceConflictItem {
                    resource_conflict_data: conflict_data,
                }),
            },
        ))
    }

    /// `CreateComLogicalLink` allocates a logical link entry and assigns a handle,
    /// but intentionally does NOT call `PassThruConnect` yet.  J2534's connect
    /// API requires a baud rate, which the ISO22900 flow supplies later via
    /// `SetComParam(DATA_RATE, …)`.  The actual `PassThruConnect` is deferred to
    /// `ConnectComLogicalLink`, by which time the baud rate is known.
    pub(super) async fn rpc_create_com_logical_link(
        &self,
        request: Request<vci_service_interface::CreateComLogicalLinkRequest>,
    ) -> Result<Response<vci_service_interface::ComLogicalLinkResponse>, Status> {
        let request = request.into_inner();
        Self::require_module_handle(request.module_handle, self.modules.len())?;
        let requested_handle = request
            .module_handle
            .expect("validated Some above")
            .module_handle;

        let resource = request.resource.ok_or_else(|| {
            Status::invalid_argument("resource is required for create_com_logical_link")
        })?;
        // ADR-196 Decision item 1: resolve and validate RawMode/ChecksumMode
        // from `cll_create_flag_bits`/`cll_create_flag_raw` -- a pure
        // function of the request itself, so this runs before any protocol
        // resolution below. The protocol-allowlist half of Decision item 1
        // (RawMode=ON restricted to base CAN/hardware ISO15765, with a
        // documented Analog Inputs/SCI no-op exception) runs further down,
        // once `hw_protocol_id`/`software_isotp` are resolved.
        let (raw_mode, checksum_mode) = resolve_cll_create_flag(request.cll_create_flag)?;
        // Legacy fallback (no resources table row matched): the pre-table
        // behavior of reading bustype/protocol names straight off `RscData`.
        let bustype_name = extract_bustype_name(&resource).unwrap_or("").to_owned();
        let protocol_name = extract_protocol_name(&resource).unwrap_or("").to_owned();
        // ADR-156 Decision 4/clause 5: whether this module opted into
        // J2534-2 gates Pin Selection (Decision 2) entirely -- read here,
        // before resolution, since `parse_protocol_id_from_resource` has no
        // `&self` to read `self.modules` from itself.
        let j2534_2_opted_in = discovery::is_j2534_2_opted_in(
            self.modules[(requested_handle - 1) as usize]
                .pname
                .as_deref(),
        );
        let (protocol, resource_row, pin_selection, channel_selection) =
            Self::parse_protocol_id_from_resource(resource, j2534_2_opted_in)?;

        // When a resources table row matched, its canonical `bus_type_name`/
        // `protocol_name` (ISO 22900-2 short names) drive the ComParam
        // defaults instead of whatever the RscData bus_type_name/protocol_name
        // fields happened to carry -- e.g. a plain numeric `resource_id` now
        // still gets the right defaults, which it never did before the table
        // existed.
        // The raw, caller-named hardware protocol id survives a no-resource-
        // table-row connect only in `pin_selection`'s own `ps_protocol_id` --
        // `protocol` itself gets normalized to the BASE `ChannelProtocol` for
        // a directly-named `_PS` id with no row match
        // (`parse_protocol_id_from_resource`), so a check keyed on
        // `protocol.j2534_protocol_id()` alone would silently never fire for
        // SWCAN/FT-CAN specifically: `resources::base_protocol_id` has
        // explicit arms collapsing their raw `_PS` ids onto the generic
        // CAN/ISO15765 family, so `protocol.j2534_protocol_id()` returns that
        // generic id, not the raw one. UART Echo Byte/Honda DIAG-H/SAE
        // J1708/SAE J1939 all have an IDENTITY `base_protocol_id` (no
        // dedicated normalization arm -- `base_protocol_id` falls through to
        // its identity fallback for all four), so a naive
        // `protocol.j2534_protocol_id()`-keyed check would have worked for
        // them too; `pin_selection`'s raw id is used uniformly across all six
        // families anyway because it is simpler than special-casing SWCAN/
        // FT-CAN, not because all six need it to avoid the normalization
        // loss. Falling back to `protocol.j2534_protocol_id()` when
        // `pin_selection` is absent preserves the prior Honda DIAG-H/SAE
        // J1708 behavior exactly (their own no-row route always yields
        // `pin_selection = Some((raw, raw, _))` today).
        //
        // Codex review finding on PR #124 (ADR-211): a directly-named
        // clause-7 `_CHx` id (e.g. `PROTOCOL_FT_CAN_CH1`) has no J1962 pin
        // concept at all -- `pin_selection` is always `None` for it -- so
        // this raw id must also be recovered from `channel_selection`
        // (whose second tuple element carries the qualified `_CHx` id
        // itself), the same way `hw_protocol_id` just below already does.
        // Without this, a CAN-collapse family whose `ChannelProtocol` is
        // REUSED from its base family (SWCAN, FT-CAN, and eventually CAN
        // FD -- unlike the six families discussed above, each of which
        // self-identifies via its own dedicated `ChannelProtocol` variant)
        // falls to `protocol.j2534_protocol_id()`, which normalizes a
        // `_CHx` connect straight to the generic `CAN`/`ISO15765` id (per
        // the SWCAN/FT-CAN normalization note above). That generic id
        // never matches `is_ft_family_protocol_id`/`is_sw_family_protocol_id`, so
        // `bustype_default_name_for_hw_protocol_id` returns `None` and the
        // connect silently gets no bustype ComParam defaults at all --
        // baud rate `0` instead of the FT bustype's own real default
        // (125,000, `ft_can.rs`'s `FTCAN_BAUD_RATE`). Adding the
        // `channel_selection` fallback here is also safe for the six
        // already-`_CHx`-enabled standalone families discussed above: when
        // `channel_selection` is populated for one of them,
        // `pin_selection` is already `None` (same "no J1962 pins for a
        // `_CHx` id" reasoning), so previously `raw_hw_id` fell through to
        // `protocol.j2534_protocol_id()`, which already correctly
        // self-identified as that family's own `_PS` id; with this change
        // `raw_hw_id` instead becomes the exact qualified `_CHx` id
        // directly, which every one of those families' own family-wide
        // predicates (`is_j1939_protocol_id`,
        // `is_uart_echo_byte_family_protocol_id`,
        // `is_honda_diagh_family_protocol_id`, `is_j1708_family_protocol_id`,
        // `is_tp2_0_family_protocol_id`, `is_gm_uart_protocol_id`) already
        // match across their full `_PS`/`_CH1..128` range -- so this is a
        // strict improvement (a more precise `raw_hw_id`), not a behavior
        // change, for those six.
        //
        // `bustype_name` here is caller-supplied `RscData` with no
        // cross-validation against `protocol` at all -- either absent (empty
        // string, `bustype_default_params("")` returns `None`) or, worse, a
        // well-formed but *mismatched* name naming a different bus type
        // entirely, which the fallback `None` arm below would otherwise
        // resolve successfully to the WRONG bustype defaults (e.g. an
        // incorrect baud rate, or `PassThruConnect` receiving `baud_rate = 0`
        // when `bustype_name` is empty). Resolving the protocol's own
        // identity FIRST -- via
        // `resources::bustype_default_name_for_hw_protocol_id`, keyed off the
        // raw hardware id above -- closes both gaps at once: an absent
        // `bustype_name` and a mismatched one are both overridden by the
        // protocol's own fixed bustype identity, which is authoritative for
        // these standalone `_PS` families regardless of whatever string
        // `RscData` happened to carry. `resolve_pin_selection`'s own
        // closed-set pin check (`names.rs`) already requires explicit,
        // validated pins for this no-row route, so this only ever applies to
        // a genuinely accepted connect, never one that would otherwise be
        // rejected.
        let raw_hw_id = pin_selection
            .map(|(_, ps_protocol_id, _)| ps_protocol_id)
            .or_else(|| channel_selection.map(|(_, chx_protocol_id, _)| chx_protocol_id))
            .unwrap_or_else(|| protocol.j2534_protocol_id());
        // ADR-156 Decision 2/3: a resolved Pin Selection or Additional
        // Channel qualifier (mutually exclusive -- `names::resolve_channel_
        // selection` rejects a request supplying both) takes top priority --
        // it already decided the `_PS`/`_CHx` hardware variant is what this
        // CLL connects with. Otherwise, a matched row's `hw_protocol_override`
        // (spec correction -- the four SAE_J2610_on_SAE_J2610_SCI rows) takes
        // priority over the CAN-channel-mode-derived default: SCI is
        // unaffected by `can_channel_mode`/J1850 auto-detect either way, so
        // this cannot conflict with either mechanism. Let-bound (rather than
        // inlined directly into the `LogicalLinkState` literal below, as
        // before this ADR) so the ADR-196 RawMode protocol-allowlist check
        // just below can read the exact same resolved value the literal
        // itself uses.
        let hw_protocol_id = pin_selection
            .map(|(_, ps_protocol_id, _)| ps_protocol_id)
            .or_else(|| channel_selection.map(|(_, chx_protocol_id, _)| chx_protocol_id))
            .unwrap_or_else(|| {
                resource_row
                    .and_then(|row| row.hw_protocol_override)
                    .unwrap_or_else(|| self.can_channel_mode.hw_protocol_id(protocol))
            });
        // ADR-157 accepted residual, extended to `_CHx` by ADR-156 Decision 3
        // addendum/Phase 2b: a `_PS`/`_CHx` link's hardware channel is the
        // qualified variant, not raw CAN, so software ISO-TP (which assumes a
        // raw CAN channel underneath) never applies to it, regardless of the
        // configured `can_channel_mode` -- an `ISO15765_PS`/`ISO15765_CHx`
        // link always uses hardware ISO-TP. Let-bound for the same ADR-196
        // reason as `hw_protocol_id` just above.
        let software_isotp = pin_selection.is_none()
            && channel_selection.is_none()
            && self.can_channel_mode.is_software_isotp(protocol);
        // ADR-196 Decision item 1 (protocol allowlist), extended by ADR-198
        // Phase 2 and ADR-200 Phase 3: RawMode=ON is accepted for a base CAN
        // or hardware ISO15765 link -- i.e. `hw_protocol_id` exactly
        // `CAN`/`ISO15765` (NOT a `_PS`/`_CHx` qualified variant: SW/FT/FD-CAN
        // all normalize away from these two exact values via
        // `resources::base_protocol_id`, but this phase deliberately does not
        // extend RawMode to them) and not running in software ISO-TP mode
        // (`!software_isotp`, checked separately since a software-ISO-TP
        // link's `hw_protocol_id` is the raw CAN channel underneath --
        // exactly `CAN` -- which would otherwise pass the bare
        // `hw_protocol_id` check above despite ADR-046 layering ISO15765
        // entirely in software over it); for hardware K-line --
        // `hw_protocol_id` exactly `ISO9141`/`ISO14230` (again NOT a `_PS`
        // qualified variant, same reasoning), software ISO-TP has no K-line
        // analog so no separate guard is needed there (ADR-198); and now ALSO
        // for SAE J1850 (`J1850VPW`/`J1850PWM` -- the generic RawMode
        // passthrough already handles J1850's TX/RX shape correctly with no
        // protocol-specific code, but a J1850 RawMode=ON CLL additionally
        // REQUIRES ChecksumMode=ON, checked separately just below, since v04.04
        // has no J1850 CRC-suppression connect flag) and SAE J1939
        // (`PROTOCOL_J1939_PS` -- via a dedicated DA-byte TX/RX shim,
        // `tx_header::build_tx_message`/`events.rs`, since the D-PDU raw wire
        // shape (4-byte CAN ID, ISO 22900-2:2022 line 774/Table 80) and the
        // native J2534-2 wire shape (5-byte CAN-ID+DA, SAE J2534-2
        // §16.4.3/Table 62) genuinely differ for this one protocol) -- ADR-200.
        // Analog Inputs (clause 10, `resources::is_analog_in_protocol_id`)
        // and SAE J2610 SCI (`SCI_A_ENGINE`/`_A_TRANS`/`_B_ENGINE`/`_B_TRANS`)
        // are a documented no-op exception: their TX path is already a raw
        // passthrough (ADR-050) and `events::header_footer_len` already
        // returns `(0, 0)` for both, so RawMode ON/OFF are observably
        // identical for them already -- accepted rather than rejected. Every
        // other protocol (UART-family, SAE J1708, TP2.0) rejects RawMode=ON
        // here. TP2.0 is a PERMANENT exclusion, not a residual (ADR-200): ISO
        // 22900-2:2022 defines no RawMode wire shape for TP2.0 at all (no
        // Table 80 row), and this service's own TP2.0 dispatch logic
        // rewrites the live, service-negotiated TX-ID at every send
        // (`events.rs`, ADR-188) -- a client-owned raw header would either be
        // silently overwritten (defeating RawMode's own premise) or go stale,
        // with no D-PDU-visible way for the client to learn the negotiated ID
        // in the first place; the broadcast path is no cleaner, since its own
        // `TP2_0_BROADCAST_MSG` TxFlag is itself ComParam-derived
        // (`tp20_broadcast_address()`), so a "raw" CLL that still needs a
        // ComParam-driven flag to send correctly isn't actually raw.
        if raw_mode {
            let raw_mode_allowed =
                (matches!(hw_protocol_id, j2534_0404::CAN | j2534_0404::ISO15765)
                    && !software_isotp)
                    || matches!(hw_protocol_id, j2534_0404::ISO9141 | j2534_0404::ISO14230)
                    || matches!(hw_protocol_id, j2534_0404::J1850VPW | j2534_0404::J1850PWM)
                    || hw_protocol_id == j2534_0404::PROTOCOL_J1939_PS
                    || resources::is_analog_in_protocol_id(hw_protocol_id)
                    || matches!(
                        hw_protocol_id,
                        j2534_0404::SCI_A_ENGINE
                            | j2534_0404::SCI_A_TRANS
                            | j2534_0404::SCI_B_ENGINE
                            | j2534_0404::SCI_B_TRANS
                    );
            if !raw_mode_allowed {
                return Err(state_guard_status(
                    Code::Unimplemented,
                    "PDU_ERR_ID_NOT_SUPPORTED: RawMode (CllCreateFlag byte 0 bit 7 / \
                     CLL_CREATE_FLAG_RAW_MODE) is supported in this phase only for a base CAN, \
                     hardware ISO15765, hardware K-line (ISO9141/ISO14230), SAE J1850 (VPW/PWM), \
                     or SAE J1939 ComLogicalLink (Analog Inputs/SCI accept it as a no-op); TP2.0 \
                     is a permanent exclusion (ISO 22900-2:2022 defines no RawMode wire shape for \
                     it, and this service's own TX-ID rewriting at every send is fundamentally \
                     incompatible with a client-owned raw header); every other protocol, and any \
                     ComLogicalLink resolving to software ISO-TP, must be created with RawMode \
                     OFF",
                    PduError::PduErrIdNotSupported,
                    None,
                ));
            }
            // ADR-200: a SAE J1850 (VPW/PWM) RawMode=ON CLL additionally
            // requires ChecksumMode=ON -- distinct from the allowlist
            // rejection above (RawMode itself IS supported for J1850; this
            // specific ChecksumMode combination is not). SAE J2534-1 v04.04
            // §7.2.3.3 Figure 7's connect-Flags table has exactly one
            // checksum-control bit, `ISO9141_NO_CHECKSUM`, scoped explicitly
            // to ISO9141/ISO14230 -- there is no analogous flag for J1850, so
            // the interface unconditionally computes/verifies/strips J1850's
            // CRC regardless of any client wish. ChecksumMode=ON's Table D.6
            // semantics (interface still manages the checksum) is exactly
            // what the device already does, so it is the only honest choice;
            // ChecksumMode=OFF's semantics (client owns the checksum) cannot
            // actually be honored for J1850 in v04.04, and silently giving
            // ON's behavior anyway would recreate the exact
            // silently-ignored-flag defect ADR-196 exists to close.
            if matches!(hw_protocol_id, j2534_0404::J1850VPW | j2534_0404::J1850PWM)
                && !checksum_mode
            {
                return Err(state_guard_status(
                    Code::Unimplemented,
                    "PDU_ERR_ID_NOT_SUPPORTED: RawMode (CllCreateFlag byte 0 bit 7) on a SAE \
                     J1850 (VPW/PWM) ComLogicalLink requires ChecksumMode ON (CllCreateFlag byte \
                     0 bit 6 / CLL_CREATE_FLAG_CHECKSUM_MODE) -- SAE J2534-1 v04.04 has no J1850 \
                     CRC-suppression connect flag (unlike ISO9141_NO_CHECKSUM for K-line), so the \
                     interface always computes/verifies/strips J1850's CRC regardless of \
                     ChecksumMode; RawMode + ChecksumMode OFF cannot be honored on this protocol \
                     and is rejected rather than silently behaving like ChecksumMode ON",
                    PduError::PduErrIdNotSupported,
                    None,
                ));
            }
        }
        let mut working = match resource_row {
            Some(row) => comparam_defaults::bustype_default_params(row.bus_type_name),
            None => resources::bustype_default_name_for_hw_protocol_id(raw_hw_id).map_or_else(
                || comparam_defaults::bustype_default_params(&bustype_name),
                comparam_defaults::bustype_default_params,
            ),
        }
        .unwrap_or_default();
        let protocol_defaults_name = resource_row
            .map(|row| row.protocol_name)
            .unwrap_or(protocol_name.as_str());
        if let Some(proto_defaults) =
            comparam_defaults::protocol_default_params(protocol_defaults_name)
        {
            working.unum32.extend(proto_defaults.unum32);
            working.bytes.extend(proto_defaults.bytes);
            working.structfield.extend(proto_defaults.structfield);
        }
        let cll_handle = self.next_logical_link_handle().await;

        // Ensure the device is open (specifically `requested_handle`'s
        // configured entry, not "whatever is already open" -- ADR-107
        // follow-up fix) so that handle allocation is tied to an open device,
        // but PassThruConnect is deferred to ConnectComLogicalLink.
        //
        // Hold the returned guard across the `logical_links` insert below
        // (fix for an orphan-CLL race found on PR #110's holistic audit):
        // without holding it continuously, a concurrent `ModuleDisconnect`
        // completing in the gap between "device confirmed open" and "CLL
        // recorded" could leave a CLL in `logical_links` whose device was
        // already closed (or, worse, belongs to a different module by the
        // time of the insert). A prior version of this fix re-acquired
        // `device_id` via a second `lock_device_for` call after
        // `ensure_open_device_for` released its own guard, then rejected if
        // nothing was open in the gap between the two acquisitions; now that
        // `ensure_open_device_for` returns its guard instead of dropping it,
        // that second acquisition (and the gap it was needed to detect) is
        // structurally impossible, so the re-check is gone too (ADR-107
        // addendum (i), Codex review on PR #110).
        let (slot, _device_id) = self.ensure_open_device_for(requested_handle).await?;

        // ADR-131 (Codex review, PR #143, round 3 on the same mechanism):
        // reject here, still holding `slot`, if a hard channel error has
        // marked this module `PduModstNotAvail` -- otherwise a client could
        // bypass ModuleConnect's now-sticky NotAvail rejection entirely by
        // instead calling CreateComLogicalLink (which never checked
        // `module_state.status`) followed by ConnectComLogicalLink, whose
        // `spawn_new_shared_channel` (below, on a genuinely fresh physical
        // channel) resets `module_state` back to `PduModstReady` as a side
        // effect of a real native `PassThruConnect` succeeding. ISO 22900-2
        // Table 9 (§9.4.9, `PDUCreateComLogicalLink`) lists
        // `PDU_ERR_MODULE_NOT_CONNECTED` as a valid return, unlike
        // `PDUModuleConnect`'s own table (hence the different error code
        // from `rpc_module_connect`'s `PDU_ERR_FCT_FAILED`).
        {
            let module_state = self.module_state.lock().await;
            if module_state.status != vci_service_interface::PduModuleStatus::PduModstReady {
                // module_not_avail_status, not module_not_connected_status: the
                // device IS open here (Codex review, PR #143) -- "call
                // ModuleConnect first" would be wrong and would leave the
                // client stuck, since ModuleConnect itself now rejects this
                // exact state too (ADR-131). Only ModuleDisconnect then
                // ModuleConnect recovers it.
                let last_error = module_state.last_error.clone();
                return Err(Self::module_not_avail_status(requested_handle, last_error));
            }
        }

        // ADR-115 (round 6: seeds `live_sender` directly rather than a
        // generation): mirror-image reconciliation for the "subscribed
        // before the CLL existed" ordering -- `SubscribeEvent` does not
        // require `cll_handle` to already be a live CLL, so a client's
        // subscription may have already installed a `SubscriptionSender` for
        // this exact key before this CLL is recorded below. Seed the new
        // queue's `live_sender` from that entry, if present, so the very
        // first `deliver_or_enqueue` call for this CLL delivers to it live
        // instead of only enqueueing (`None`, the default, when no such
        // subscription exists yet).
        //
        // The seed-read and the `logical_links` insert are ONE atomic
        // critical section (lock `logical_links`, then nested-lock
        // `subscriptions` -- this crate's stated lock order, `logical_links`
        // -> `subscriptions`) rather than two separate lock acquisitions: as
        // two separate critical sections, a `SubscribeEvent` call's own
        // reconciliation (which re-checks `logical_links` after its
        // `subscriptions` insert) could miss this not-yet-inserted link, AND
        // this seed-read could predate that same call's `subscriptions`
        // insert -- leaving both sides permanently unreconciled, the same bug
        // shape `rpc_subscribe_event` itself avoids by making its own
        // map-insert and queue-stamp atomic (see that function's own doc
        // comment).
        let mut links = self.logical_links.lock().await;
        let initial_live_sender = self
            .subscriptions
            .lock()
            .await
            .get(&(DEFAULT_MODULE_HANDLE, cll_handle))
            .cloned();

        links.insert(
            cll_handle,
            LogicalLinkState {
                channel_id: None,
                protocol,
                // Resolved above (`hw_protocol_id`/`software_isotp` let
                // bindings) so the ADR-196 RawMode protocol-allowlist check
                // could read the same values before this literal is built --
                // see those bindings' own doc comments for the unchanged
                // ADR-156/ADR-157 resolution logic.
                hw_protocol_id,
                software_isotp,
                uudt_channel_id: None,
                uudt_channel_key: None,
                isotp_rx: Arc::new(Mutex::new(std::collections::HashMap::new())),
                connect_in_flight: std::sync::Weak::new(),
                connected: false,
                comm_started: false,
                // ADR-196 Decision item 1: resolved and protocol-allowlist-
                // validated above.
                raw_mode,
                // ADR-198 Phase 2 Decision item 1: resolved and format-
                // validated above (`resolve_cll_create_flag`) -- meaningful
                // only for a RawMode=ON K-line CLL; ignored (treated as
                // `false`'s effect) for every other CLL, matching Table
                // D.6's own "ignored when RawMode is OFF" rule.
                checksum_mode,
                connect_generation: 0,
                stop_comm_pending: false,
                channel_key: None,
                pin_select: pin_selection.map(|(_, _, pin_select)| pin_select),
                // ADR-156 Decision 3/Phase 2b: `Some(1..=128)` exactly when
                // `channel_selection` resolved (mutually exclusive with
                // `pin_select` above).
                channel_index: channel_selection.map(|(_, _, channel_index)| channel_index),
                // ADR-157/Bug 1 fix, generalized to `_CHx` by ADR-156
                // Decision 3 addendum: the correctly-resolved base hw
                // protocol id `resolve_pin_selection`/`resolve_channel_selection`
                // already computed internally (preserving, e.g., the exact
                // SAE J2610 SCI variant) -- see
                // `LogicalLinkState::base_hw_protocol_override`'s own doc
                // comment.
                base_hw_protocol_override: pin_selection
                    .map(|(base_hw_protocol_id, _, _)| base_hw_protocol_id)
                    .or_else(|| {
                        channel_selection.map(|(base_hw_protocol_id, _, _)| base_hw_protocol_id)
                    }),
                rx_buf: Arc::new(Mutex::new(CllEventQueue {
                    items: std::collections::VecDeque::new(),
                    live_sender: initial_live_sender,
                    event_queue_cap: RX_BUF_CAPACITY,
                    event_queue_mode: EventQueueMode::default(),
                    result_buffer_limit: None,
                    next_reservation_id: 0,
                })),
                working,
                active: ComParamSet::default(),
                tester_present_state: TesterPresentState::None,
                tester_present_base_tx_flags: 0,
                open_tp_discards: Vec::new(),
                working_unique_resp_id_table: Vec::new(),
                active_unique_resp_id_table: Vec::new(),
                unique_resp_filter_ids: Vec::new(),
                cancelled_cops: std::collections::HashSet::new(),
                held_lock_mask: 0,
                last_error: None,
                tx_held: std::collections::VecDeque::new(),
                tx_suspended_by_ioctl: false,
                tx_suspended_by_lock: false,
                tx_suspended_by_error: false,
                error_clear_seq: 0,
                error_set_seq: 0,
                client_filters: std::collections::HashMap::new(),
                repeat_message_ids: Vec::new(),
                pending_client_filters: std::collections::HashMap::new(),
                registrants: Vec::new(),
                next_registrant_seq: 0,
                j1939_claimed_address: None,
                j1939_claim_cursor: 0,
                j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
                tp20_connection: None,
                tp20_broadcast_periodic: None,
            },
        );
        drop(links);
        // A2-23 (ADR-128, Codex-review round 1): clear a stale `destroyed_clls`
        // marker for `cll_handle` in case `next_logical_link_handle`'s
        // wrapping allocator just reissued a number some earlier, long-since-
        // destroyed CLL used -- without this, `TerminalCopsLedger::record`
        // would silently drop every terminal status this BRAND NEW CLL's COPs
        // ever reach, mistaking "this number was destroyed in a past life"
        // for "the CLL currently holding it is destroyed". Correctness-
        // required on that (astronomically rare) path, a no-op otherwise.
        self.terminal_cops.lock().await.unmark_destroyed(cll_handle);
        drop(slot);

        debug!(
            cll_handle,
            protocol = protocol.value(),
            "CreateComLogicalLink"
        );
        Ok(Response::new(
            vci_service_interface::ComLogicalLinkResponse {
                cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                    module_handle: DEFAULT_MODULE_HANDLE,
                    cll_handle,
                }),
            },
        ))
    }

    pub(super) async fn rpc_destroy_com_logical_link(
        &self,
        request: Request<vci_service_interface::DestroyComLogicalLinkRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;

        // Hold shared_channels across the whole remove-from-logical_links +
        // filter-teardown + ref_count sequence below (ADR-080: shared_channels
        // is the outermost lock) -- otherwise a concurrent
        // ConnectComLogicalLink's (or ensure_uudt_companion_channel's)
        // reciprocal client_filters check could see this CLL already gone
        // from `logical_links` (and thus its filters invisible) and join the
        // channel before the hardware filter is actually stopped, or before
        // ref_count reflects this CLL's departure (Codex-review fix).
        let mut chans = self.shared_channels.lock().await;

        let (link, wake_targets) = {
            let mut links = self.logical_links.lock().await;
            let link = links
                .remove(&handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;

            // ADR-123: this CLL (and whatever lock bits it held) is now gone
            // from `links` entirely -- recompute every remaining CLL's
            // tx_suspended_by_lock and collect any that transitioned to
            // unsuspended, to wake below (still inside the
            // shared_channels-guarded critical section, ADR-080).
            let resumed = recompute_lock_tx_suspensions(&mut links);
            let wake_targets: Vec<(u32, mpsc::UnboundedSender<TxItem>)> = resumed
                .into_iter()
                .filter_map(|h| {
                    links
                        .get(&h)
                        .and_then(|l| l.channel_key)
                        .and_then(|ck| chans.get(&ck).map(|sc| (h, sc.tx_queue.clone())))
                })
                .collect();
            (link, wake_targets)
        };

        for (h, tx_queue) in wake_targets {
            let _ = tx_queue.send(TxItem::ResumeWake { cll_handle: h });
        }

        // Capture channel_id before the link is consumed, for the client-filter
        // teardown below. Tester-present (ADR-083, software-driven for both
        // `CP_TesterPresentSendType` values as of this diff) has no hardware
        // resource to release -- it is simply dropped along with `link`.
        let was_connected = link.connected;
        let channel_id_for_tp = link.channel_id;

        // Stop this CLL's client-installed message filters before it goes away.
        // (ADR-010's pattern): done regardless of whether the physical channel
        // is shared or about to close -- harmless if the channel is about to
        // close anyway (PassThruDisconnect tears down all filters with it),
        // necessary when the channel is shared and stays open for another
        // CLL. A failed stop here is only logged, not retried or re-tracked --
        // safe per ADR-082: a non-empty `client_filters` implies this CLL is
        // the sole owner of its channel, so `ref_count` is about to hit 0
        // below and `PassThruDisconnect` tears down every hardware filter on
        // the channel regardless of this loop's outcome.
        //
        // Invariant (ADR-193 amendment, round 17, Codex review, P1, PR #101,
        // re-verified while auditing every `tp20_broadcast_periodic` call
        // site for unfenced terminators): `channel_id_for_tp` is `None` here
        // only if `link.tp20_broadcast_periodic` was already `None`, or was
        // already taken atomically alongside `channel_id` itself -- by ONE
        // of three sites, all of which pair a `channel_id`
        // clear/precondition with a `tp20_broadcast_periodic`
        // clear/precondition inside the SAME critical section:
        // `reserve_tp20_broadcast_periodic` (`rpc_primitive.rs`) requires a
        // live `channel_id` to reserve in the first place (so a reservation
        // can never outlive a cleared `channel_id`); `rpc_disconnect_com_
        // logical_link` (this file) takes the periodic atomically with its
        // own `channel_id` clear, in the same `logical_links` critical
        // section; and `events::handle_channel_hard_error` (`events.rs`)
        // clears `channel_id` and takes the periodic in the SAME per-CLL
        // closure, both gated on the identical `was_primary` condition (a
        // broadcast periodic is only ever started against a link's PRIMARY
        // channel_id, so the pairing is exact, not merely coincidental) --
        // so this `Some(channel_id)` gate never skips a live, still-tracked
        // `tp20_broadcast_periodic` below. If that atomicity ever decouples
        // at any of these three sites, this function needs its own
        // `self.api` fence too.
        if let Some(channel_id) = channel_id_for_tp {
            let api = self.api.lock().await;
            for filter_id in link.client_filters.values().flatten() {
                if let Err(err) = api.stop_message_filter(channel_id, *filter_id) {
                    warn!(cll_handle = handle, %err, "DestroyComLogicalLink: stop_message_filter failed");
                }
            }
            // SAE J2534-2 clause 14 Repeat Messaging (ADR-165 Decision 4/6,
            // Codex review PR #42 round 2 Finding A): best-effort STOP for
            // every repeat slot this CLL started and never stopped --
            // mirrors the client-filter loop just above (iterate, call
            // native stop, never abort teardown on one failed stop), but
            // unlike `client_filters` a failed stop here is NOT always safe
            // to just log and drop: `client_filters` non-empty guarantees
            // this CLL is the sole channel owner (ADR-082), so `ref_count`
            // is about to hit 0 below regardless. A repeat slot has no such
            // guarantee -- the slot budget is shared across sibling CLLs on
            // one physical channel (ADR-165 Decision 6) -- so when the
            // channel is NOT about to close (a sibling CLL still holds a
            // reference), a failed STOP here would otherwise orphan the slot
            // on a channel with no owner and no way to stop it via the API
            // again. `chans` (shared_channels) is held across this entire
            // function (see the lock-order comment above), so this
            // `ref_count` read is authoritative for what the decrement near
            // the end of this function will do -- no other task can join or
            // leave this channel in between.
            let repeat_channel_will_stay_open = link
                .channel_key
                .and_then(|key| chans.get(&key))
                .is_some_and(|sc| sc.ref_count > 1);
            for &msg_id in &link.repeat_message_ids {
                if let Err(err) = api.stop_repeat_message(channel_id, msg_id) {
                    warn!(cll_handle = handle, msg_id, %err, "DestroyComLogicalLink: stop_repeat_message failed");
                    if repeat_channel_will_stay_open
                        && let Some(sc) = link.channel_key.and_then(|key| chans.get_mut(&key))
                    {
                        sc.leaked_repeat_message_ids.push(msg_id);
                    }
                }
            }

            // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c):
            // best-effort stop of this CLL's own live broadcast periodic
            // message, mirroring the Repeat Messaging loop just above --
            // deliberately UNCONDITIONAL (not gated on
            // `repeat_channel_will_stay_open`, unlike the J1939-claim/TP2.0-
            // connection arms below): the stop is always attempted whether or
            // not a sibling CLL keeps the channel open, and reinstates the
            // exact ADR-010 leak class deliberately for the case that
            // matters: a sibling CLL keeping the physical channel open must
            // NOT suppress this stop call (see
            // `LogicalLinkState::tp20_broadcast_periodic`'s own doc
            // comment).
            // Fix 2 (Codex review, P1, PR #101, ADR-192/Phase 7 Stage 7c): a
            // `None`-sentinel `message_id` (`rpc_primitive.rs::
            // rpc_start_com_primitive`'s own in-flight-start reservation,
            // see `Tp20BroadcastPeriodic`'s doc comment) means no real
            // message exists yet to stop -- skip the native call rather
            // than issue a meaningless native stop with no real id.
            //
            // Codex review round 15 (P1, PR #101, ADR-193): unlike
            // `CoptCancel`/`DisconnectComLogicalLink`/the suspension
            // termination, this site needs no change to participate in the
            // `self.api` serialization fence, and the sentinel case is no
            // longer an orphan-with-nothing-to-stop-it residual. This
            // function removes the CLL from `logical_links` outright (above,
            // under `logical_links`) BEFORE acquiring `api` here, so an
            // in-flight start either has not yet revalidated -- and will find
            // its CLL gone under the fence and never call the native start --
            // or is already inside its own `api` bracket, whose in-bracket
            // resolution
            // (`finalize_or_orphan_broadcast_periodic_start_locked`) then
            // resolves `NotOwned` and stops the just-started message under
            // that same guard, which this function's own `api` acquisition
            // necessarily waits behind.
            //
            // Fix B (design-advisor consult, Codex review round 3, ADR-192
            // Decision item 2): unlike Repeat Messaging's own leak-tracking
            // above, `tp20_broadcast_periodic` DID have "no sibling-budget
            // leak-tracking concept" until this fix -- a failed stop used to
            // be a bare log-and-drop even when a sibling CLL keeps the
            // physical channel open. Now mirrors `leaked_repeat_message_ids`
            // exactly: when `repeat_channel_will_stay_open`, a real (non-
            // sentinel) `PeriodicMessageId` a failed stop leaves behind is
            // pushed onto that physical channel's own
            // `SharedChannel::leaked_periodic_message_ids` instead of being
            // silently dropped, so it keeps blocking `LOCK_PHYSICAL_TX_QUEUE`
            // grants and is retried/pruned the same way a leaked repeat slot
            // is. When the channel is NOT staying open (this CLL is the last
            // one), the imminent `PassThruDisconnect` takes the device-side
            // message with it regardless -- unchanged log-and-drop.
            if let Some(periodic) = link.tp20_broadcast_periodic
                && let Some(message_id) = periodic.message_id
                && let Err(err) = api.stop_periodic_message(channel_id, message_id)
            {
                warn!(
                    cll_handle = handle,
                    message_id = message_id.0,
                    %err,
                    "DestroyComLogicalLink: best-effort PassThruStopPeriodicMsg for a TP2.0 \
                     broadcast periodic COP failed"
                );
                if repeat_channel_will_stay_open
                    && let Some(sc) = link.channel_key.and_then(|key| chans.get_mut(&key))
                    && !sc
                        .leaked_periodic_message_ids
                        .iter()
                        .any(|&(id, _)| id == message_id)
                {
                    sc.leaked_periodic_message_ids
                        .push((message_id, periodic.started_epoch));
                }
            }

            // SAE J2534-2 clause 16 SAE J1939 (ADR-179 Decision 3):
            // best-effort cancel of every address this CLL still owns in
            // its physical channel's `SharedChannel::j1939_claims`, mirroring
            // the repeat-message loop just above -- but only while the
            // channel survives this CLL's own teardown (`PassThruDisconnect`
            // below already tears down device state, including any claim,
            // when the channel is closing for good, so no explicit cancel
            // is needed then).
            if repeat_channel_will_stay_open
                && resources::is_j1939_protocol_id(link.hw_protocol_id)
                && let Some(sc) = link.channel_key.and_then(|key| chans.get_mut(&key))
            {
                events::cancel_j1939_claims_for_cll(
                    handle,
                    link.connect_generation,
                    channel_id,
                    &api,
                    sc,
                )
                .await;
            }

            // SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a):
            // best-effort teardown of this CLL's own connection, mirroring
            // the SAE J1939 claim-cancel arm just above -- only while the
            // channel survives this CLL's own teardown (`PassThruDisconnect`
            // below already tears down device state, including any
            // connection, when the channel is closing for good). Keyed on
            // the connection's own `requested_rx_id` (clause 19.3.3.3).
            // ADR-210 Decision item 11 (the highest-severity fix in that
            // ADR): re-keyed from the narrow `is_tp2_0_protocol_id` to
            // `is_tp2_0_family_protocol_id` -- `link.hw_protocol_id` is a
            // live link's raw id, so a `_CHx`-connected TP2.0 CLL's own
            // active connection would otherwise never be torn down here,
            // leaking the native connection slot.
            if repeat_channel_will_stay_open
                && resources::is_tp2_0_family_protocol_id(link.hw_protocol_id)
                && let Some(conn) = link
                    .tp20_connection
                    .filter(|c| !c.passive && c.phase == Tp20ConnectionPhase::Established)
            {
                if let Err(err) = api.tp20_teardown_connection(channel_id, conn.requested_rx_id) {
                    warn!(
                        cll_handle = handle,
                        requested_rx_id = conn.requested_rx_id,
                        %err,
                        "DestroyComLogicalLink: best-effort TP2.0 IOCTL_TEARDOWN_CONNECTION \
                         failed -- leaked native connection slot is an accepted residual \
                         (ADR-188 Consequences)"
                    );
                }
                // Codex review fix (PR #97, round 14, corrected round 17,
                // reverted round 21): quarantine rx_id the same way
                // `handle_stop_comm`'s identical fix does -- `IOCTL_
                // TEARDOWN_CONNECTION` is non-blocking, so the device's own
                // delayed `CONNECTION_LOST` confirmation can still arrive
                // after this returns, and nothing else prevents a
                // promptly-issued new `CoptStartcomm` (on whichever sibling
                // CLL keeps this channel open) from registering its own
                // pending entry for the SAME rx_id before that confirmation
                // drains. Deliberately UNCONDITIONAL, not gated on whether
                // the teardown call above succeeded -- see `handle_stop_
                // comm`'s identical fix (`events.rs`) for the full
                // round-21 rationale: a synchronous failure here can also
                // mean the device independently and spontaneously lost this
                // connection before this teardown ran, with a stale
                // indication already queued, not that none is coming.
                if let Some(sc) = link.channel_key.and_then(|key| chans.get_mut(&key)) {
                    events::quarantine_tp20_connection_for_orphaned_write_back(
                        conn.requested_rx_id,
                        handle,
                        link.connect_generation,
                        &mut sc.tp20_connections,
                    );
                }
            }

            // ADR-190/Phase 7 Stage 7b section 4: passive disarm, mirroring
            // `handle_stop_comm`'s identical sequence (`events.rs`) --
            // reverses `arm_tp20_passive_listener` regardless of the
            // listener's own phase (`Listening` or `Established`), only
            // while the channel survives this CLL's own teardown (the same
            // `repeat_channel_will_stay_open` gate the active-connection arm
            // above uses).
            // ADR-210 Decision item 11: re-keyed from the narrow
            // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id`, the
            // same leak-closing fix as the active-connection arm above,
            // applied to the passive-listener-disarm path.
            if repeat_channel_will_stay_open
                && resources::is_tp2_0_family_protocol_id(link.hw_protocol_id)
                && let Some(conn) = link.tp20_connection.filter(|c| c.passive)
            {
                let rx_id_passive = conn.requested_rx_id;
                let was_established = conn.phase == Tp20ConnectionPhase::Established;
                let config_cleared =
                    events::best_effort_disarm_tp20_passive_native_config(&api, channel_id, handle);
                if was_established
                    && let Err(err) = api.tp20_teardown_connection(channel_id, rx_id_passive)
                {
                    warn!(
                        cll_handle = handle,
                        rx_id_passive,
                        %err,
                        "DestroyComLogicalLink: best-effort TP2.0 IOCTL_TEARDOWN_CONNECTION on \
                         passive disarm failed -- leaked native connection slot is an accepted \
                         residual (ADR-190 Consequences), or the whole call is spec-ambiguous on \
                         a real adapter and self-heals via the maintenance timeout"
                    );
                }
                // `was_established`/`config_cleared` gate a bounded release
                // deadline for the never-`Established` disarm case (ADR-190's
                // Correction paragraph, Codex review finding, P1, PR #99) --
                // see `quarantine_tp20_passive_slot_on_disarm`'s own doc
                // comment.
                if let Some(sc) = link.channel_key.and_then(|key| chans.get_mut(&key)) {
                    events::quarantine_tp20_passive_slot_on_disarm(
                        rx_id_passive,
                        sc,
                        was_established,
                        config_cleared,
                    );
                }
            }
        }

        // Cancel any COPs that were queued but will never execute now that the link
        // is being destroyed.  This must happen before terminate_subscription so that
        // the PduCopstCancelled events can still reach the subscriber.
        events::cancel_link_cops(
            &self.primitives,
            &self.logical_links,
            &self.subscriptions,
            &self.terminal_cops,
            handle,
        )
        .await;

        // A2-23 (ADR-128): this CLL is now fully gone -- drop every terminal-status
        // ledger entry that belonged to it, and mark the handle destroyed so a
        // straggling terminal emission that raced this purge (won the
        // `primitives` removal before `cancel_link_cops` got there, but hasn't
        // yet reached its own `send_cop_status` call) cannot resurrect an entry
        // afterward (`TerminalCopsLedger::purge`, Codex-review round 1). A COP
        // handle is never reused within a CLL's lifetime (see
        // `next_primitive_handle`'s allocator) and this CLL itself is gone from
        // `logical_links`, so nothing can observe a purged entry again; leaving
        // entries around would grow `terminal_cops` unboundedly across a
        // long-running module's CLL churn.
        self.terminal_cops.lock().await.purge(handle);

        // Emit CLL state transition events before closing the subscription stream.
        // The subscription is still open at this point (terminate_subscription is
        // called below), so these events reach the subscriber.
        // ISO 22900-2's any-state-to-Offline use case: go directly to Offline from
        // any state — no intermediate Online step regardless of comm_started.
        if was_connected {
            events::send_cll_status(
                &self.subscriptions,
                &self.logical_links,
                handle,
                vci_service_interface::PduComLogicalLinkStatus::PduCllstOffline,
            )
            .await;
        }

        if let Some(key) = link.channel_key {
            let disconnected = if let Some(sc) = chans.get_mut(&key) {
                sc.ref_count -= 1;
                if sc.ref_count == 0 {
                    chans.remove(&key).map(|sc| {
                        // SAE J2534-2 clause 14 Repeat Messaging backstop
                        // (ADR-165 Decision 6, Codex review PR #42 round 2
                        // Finding A): this channel is closing for good --
                        // `PassThruDisconnect` below tears down every repeat
                        // slot on it along with the channel, so any MsgIds
                        // still tracked as leaked here are dropped now
                        // rather than retried.
                        if !sc.leaked_repeat_message_ids.is_empty() {
                            debug!(
                                channel_id = sc.channel_id.0,
                                leaked_msg_ids = ?sc.leaked_repeat_message_ids,
                                "DestroyComLogicalLink: dropping leaked repeat-message MsgIds, channel is closing"
                            );
                        }
                        // ADR-180 Decision 22: same backstop, one level up
                        // -- see `SharedChannel::leaked_j1939_claims`'s own
                        // field doc (`service.rs`) for why any remaining
                        // entry is moot once the channel itself is gone.
                        if !sc.leaked_j1939_claims.is_empty() {
                            debug!(
                                channel_id = sc.channel_id.0,
                                leaked_addresses = ?sc.leaked_j1939_claims,
                                "DestroyComLogicalLink: dropping leaked SAE J1939 claimed addresses, channel is closing"
                            );
                        }
                        // Fix B (design-advisor consult, Codex review round
                        // 3, ADR-192 Decision item 2): same backstop, one
                        // level up again -- see
                        // `SharedChannel::leaked_periodic_message_ids`'s own
                        // field doc (`service.rs`) for why any remaining
                        // entry is moot once the channel itself is gone.
                        if !sc.leaked_periodic_message_ids.is_empty() {
                            debug!(
                                channel_id = sc.channel_id.0,
                                leaked_message_ids = ?sc.leaked_periodic_message_ids,
                                "DestroyComLogicalLink: dropping leaked TP2.0 broadcast periodic \
                                 MessageIds, channel is closing"
                            );
                        }
                        sc.channel_id
                    })
                } else {
                    None
                }
            } else {
                None
            };
            drop(chans);
            if let Some(channel_id) = disconnected {
                // ADR-101 Decision §E: a torn-down physical channel's drain
                // watermark is stale hygiene, not a correctness requirement
                // (a reused ChannelId's stale entry can only ever cause an
                // extra defer, never a wrongful permit -- see that Decision)
                // -- cleared here anyway so the map doesn't grow unboundedly
                // over a long-running service's connect/disconnect cycles.
                self.drain_watermarks.lock().await.remove(&channel_id);
                let api = self.api.lock().await;
                api.disconnect(channel_id).map_err(|err| {
                    map_native_error_for_link("PassThruDisconnect", &err, link.last_error)
                })?;
                // ADR-192/Phase 7 Stage 7c reinstated
                // `PassThruStartPeriodicMsg`/`PassThruStopPeriodicMsg` for
                // the TP2.0 broadcast-periodic-re-trigger case (superseding
                // this comment's prior ADR-083 claim that this service never
                // starts periodic messages) -- but `PassThruDisconnect` just
                // above already tears down any live or leaked periodic
                // message on this channel along with the channel itself, so
                // there is still nothing further to stop here.
            }
            // Not the last CLL (physical channel stays open): tester-present
            // teardown for this CLL is a plain state clear, already done by
            // `link` going out of scope above -- no hardware call needed
            // (ADR-083/this diff).
        } else {
            drop(chans);
        }
        // Release the dual-channel-mode UUDT companion channel, if any (ADR-046).
        if let Some(key) = link.uudt_channel_key {
            self.release_shared_channel_ref(key, None).await;
        }
        // `link` (removed from `logical_links` above, still in scope) is the
        // only remaining handle onto this CLL's queue -- pass its `Arc`
        // through directly rather than letting `terminate_subscription`
        // re-look-up a handle that's already gone from `logical_links`.
        self.terminate_subscription((DEFAULT_MODULE_HANDLE, handle), Some(&link.rx_buf))
            .await;

        debug!(cll_handle = handle, "DestroyComLogicalLink");
        Ok(Self::empty_response())
    }

    /// Handles the first CLL connecting on a not-yet-existing physical channel:
    /// checks `LOCK_PHYSICAL_COM_PARAMS` (ADR-045, no `channel_key` exists yet so
    /// this matches on `j2534_proto_id` alone), calls `PassThruConnect`, applies
    /// the Working ComParam set to hardware, and — for non-ISO15765 channels
    /// only — installs a pass-all filter so the service starts receiving frames.
    ///
    /// ISO15765 channels get no filter here: `rpc_connect_com_logical_link`
    /// builds point-to-point `FLOW_CONTROL_FILTER`s directly from the CLL's
    /// UniqueRespIdTable once it is finalized, whether that table is empty or
    /// already configured (ADR-048).
    ///
    /// Joining CLLs onto an already-connected channel never call this: they
    /// apply nothing at connect time, and their Working set only reaches
    /// hardware later via an explicit `CoptUpdateparam` -- which, per ADR-110,
    /// no longer synchronously checks this lock at all: a live conflict on a
    /// `PDU_PC_BUSTYPE`-class param instead resolves as a
    /// `PDU_ERR_EVT_RSC_LOCKED` error event at execution time, with the COP
    /// still finishing normally.
    ///
    /// On any hardware failure after `PassThruConnect` succeeds, the channel is
    /// disconnected before the error is returned, so no channel is left dangling.
    async fn connect_new_physical_channel(
        &self,
        handle: u32,
        params: NewPhysicalChannelParams<'_>,
    ) -> Result<ChannelId, Status> {
        let NewPhysicalChannelParams {
            device_id,
            j2534_proto_id,
            baud_rate,
            connect_flags,
            working_snapshot,
            pin_select,
            fd_data_phase_rate,
            native_mixed_format,
            analog_sample_rate,
        } = params;
        // ADR-157 Plane B: every family/behavior decision below (ComParam
        // support, the pass-all-filter gate) must use the base protocol id,
        // even though `j2534_proto_id` itself (the actual `PassThruConnect`/
        // `SET_CONFIG` argument, Plane A) stays the raw `_PS` id throughout
        // this function.
        let base_proto_id = resources::base_protocol_id(j2534_proto_id);
        let (lock_conflict, last_error_at_lock_check) = {
            let links = self.logical_links.lock().await;
            let conflict = find_physical_lock_holder(
                &links,
                handle,
                j2534_proto_id,
                pin_select,
                None,
                LOCK_PHYSICAL_COM_PARAMS,
            );
            let last_error = links.get(&handle).and_then(|l| l.last_error.clone());
            (conflict, last_error)
        };
        if let Some((holder, held)) = lock_conflict {
            return Err(state_guard_status(
                Code::ResourceExhausted,
                format!(
                    "cll_handle {holder} already holds the physical ComParam lock (mask {held:#04x}) on this resource",
                ),
                PduError::PduErrRscLockedByOtherCll,
                last_error_at_lock_check,
            ));
        }

        // `last_error` is read fresh at each failure point below, after `api` is
        // released (dropping it first where a rollback call needed it), rather
        // than snapshotted once up front: a pre-call snapshot can miss a
        // hard-channel-error update the poll task makes while this RPC is still
        // waiting for `self.api`'s lock (Codex review, ADR-105). `self.api` and
        // `self.logical_links` are never held simultaneously here -- there is no
        // established ordering between them elsewhere in this crate.
        let api = self.api.lock().await;
        let connect_result = api.connect(device_id, j2534_proto_id, connect_flags, baud_rate);
        let channel_id = match connect_result {
            Ok(channel_id) => channel_id,
            Err(err) => {
                warn!(cll_handle = handle, j2534_proto_id, baud_rate, connect_flags, %err, "PassThruConnect failed");
                drop(api);
                let last_error = self
                    .logical_links
                    .lock()
                    .await
                    .get(&handle)
                    .and_then(|l| l.last_error.clone());
                // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
                // `ERR_NO_CONNECTION_ESTABLISHED` (the native activation-line
                // failure) maps to `PDU_ERR_NO_CABLE_DETECTED` rather than the
                // generic `PDU_ERR_FCT_FAILED` catch-all -- but only at THIS
                // call site, scoped to an Ethernet_NDIS `PassThruConnect`.
                // This native code already has a different, unrelated
                // meaning elsewhere (the mock's TP2.0 oversized-write path,
                // ADR-188), so a global `pdu_error_for` table entry would be
                // wrong; `map_native_error_as` with an explicit override is
                // used instead, mirroring `ioctl_set_prog_voltage`'s own
                // native-code-scoped override pattern (A2-21).
                if base_proto_id == j2534_0404::PROTOCOL_ETHERNET_NDIS
                    && matches!(
                        &err,
                        j2534_0404::Error::ApiStatus { code, .. }
                            if code.as_u32() == j2534_0404::ERR_NO_CONNECTION_ESTABLISHED
                    )
                {
                    return Err(map_native_error_as(
                        "PassThruConnect",
                        &err,
                        PduError::PduErrNoCableDetected,
                        last_error,
                    ));
                }
                return Err(map_native_error_for_link(
                    "PassThruConnect",
                    &err,
                    last_error,
                ));
            }
        };

        // SAE J2534-2 clause 21 CAN FD (ADR-158): `FD_CAN_DATA_PHASE_RATE`
        // must be SET_CONFIG'd before `CONFIG_J1962_PINS` below -- clause
        // 21.3.2.5.1's mandatory ordering -- else the native connect fails
        // `ERR_FAILED`. `None` for every non-FD connect skips this step
        // entirely; a conditional insertion, not an unconditional reorder.
        if let Some(rate) = fd_data_phase_rate
            && let Err(err) =
                api.set_config_u32(channel_id, j2534_0404::CONFIG_FD_CAN_DATA_PHASE_RATE, rate)
        {
            let _ = api.disconnect(channel_id);
            drop(api);
            let last_error = self
                .logical_links
                .lock()
                .await
                .get(&handle)
                .and_then(|l| l.last_error.clone());
            return Err(map_native_error_for_link(
                "PassThruIoctl SET_CONFIG (CONFIG_FD_CAN_DATA_PHASE_RATE)",
                &err,
                last_error,
            ));
        }

        // SAE J2534-2 clause 6 Pin Selection (ADR-156 Decision 2)/clause 21
        // CAN FD (ADR-158): a `_PS`/FD link's DLC pins are unassigned until
        // this `SET_CONFIG` -- issued right after `PassThruConnect` (and,
        // for FD, the data-phase-rate step above) and before any other
        // ComParam application, so the pins are already bound by the time
        // this physical channel is usable at all. Only reached on first-open
        // of the physical channel (this function is never called for a CLL
        // joining an already-open shared channel -- see
        // `rpc_connect_com_logical_link`'s join-vs-create branch), so a
        // second CLL sharing the same `pin_select` (and therefore the same
        // widened `ChannelKey`) never re-issues it. `None` for every
        // non-`_PS`/non-FD link (the overwhelming majority) skips this step
        // entirely, unchanged from pre-Phase-2a behavior. ADR-160
        // Correction: this step must also precede the
        // `CONFIG_CAN_MIXED_FORMAT` step below on a qualified link -- clause
        // 6.3.2.7 rejects every `PassThruIoctl` other than the
        // pin-assignment `SET_CONFIG` itself with `PDU_ERR_PIN_INVALID`
        // until a `_PS`/`_CHx`-qualified channel's pins are assigned, and
        // clause 8 (SS8.2.1) grants `CAN_MIXED_FORMAT` no exception to that
        // rule (unlike the FD data-phase-rate step above, which clause
        // 21.3.2.5.1 explicitly exempts from this ordering).
        if let Some(pin_select) = pin_select
            && let Err(err) =
                api.set_config_u32(channel_id, j2534_0404::CONFIG_J1962_PINS, pin_select)
        {
            let _ = api.disconnect(channel_id);
            drop(api);
            let last_error = self
                .logical_links
                .lock()
                .await
                .get(&handle)
                .and_then(|l| l.last_error.clone());
            return Err(map_native_error_for_link(
                "PassThruIoctl SET_CONFIG (CONFIG_J1962_PINS)",
                &err,
                last_error,
            ));
        }

        // SAE J2534-2 clause 8 Mixed Format Frames on a CAN Network
        // (ADR-160): enables native USDT+UUDT mixing on this ISO15765-family
        // channel instead of relying on the ADR-041 FLOW_CONTROL_FILTER UUDT
        // workaround (`install_point_to_point_fc_filters`'s `native_mixed`
        // branch installs a genuine PASS_FILTER per UUDT id instead).
        // ADR-160 Correction: issued after pin selection above, not right
        // after the FD data-phase-rate/analog-sample-rate steps as
        // originally implemented -- clause 6.3.2.7 rejects every
        // `PassThruIoctl` other than the pin-assignment `SET_CONFIG` itself
        // with `PDU_ERR_PIN_INVALID` until a `_PS`/`_CHx`-qualified
        // channel's pins are assigned, and unlike the FD data-phase-rate
        // step (explicitly exempted by clause 21.3.2.5.1), clause 8
        // (SS8.2.1) has no such carve-out for `CAN_MIXED_FORMAT`, so pins
        // must be bound first on a qualified link. `None` for every
        // non-native-mixed/FD-substituted connect skips this step entirely;
        // `Some(value)` sends `CAN_MIXED_FORMAT_ON` or
        // `CAN_MIXED_FORMAT_ALL_FRAMES` depending on which native-mixed
        // sub-mode resolved (ADR-217). `ERR_NOT_SUPPORTED` (e.g. a device
        // that doesn't implement clause 8) fails the connect outright,
        // mirroring the FD data-phase-rate step's own rollback-on-failure
        // shape -- no silent fallback to the ADR-041 workaround.
        if let Some(value) = native_mixed_format
            && let Err(err) =
                api.set_config_u32(channel_id, j2534_0404::CONFIG_CAN_MIXED_FORMAT, value)
        {
            let _ = api.disconnect(channel_id);
            drop(api);
            let last_error = self
                .logical_links
                .lock()
                .await
                .get(&handle)
                .and_then(|l| l.last_error.clone());
            return Err(map_native_error_for_link(
                "PassThruIoctl SET_CONFIG (CONFIG_CAN_MIXED_FORMAT)",
                &err,
                last_error,
            ));
        }

        // Apply all remaining Working params (DATA_RATE already set by connect).
        // On failure, disconnect the channel before returning to avoid a resource leak.
        // ADR-158: `j2534_proto_id` (Plane A, raw -- may be `_PS`/FD) is
        // passed here rather than `base_proto_id`, so `to_j2534_config_id`
        // can see an FD_CAN_PS link directly and suppress
        // `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` (clause 21.3.2.5.1) --
        // `apply_j2534_params` normalizes internally (via
        // `resources::base_protocol_id`) for `expand_tidle`'s own
        // family-arm dispatch, so this is not a regression for any
        // non-FD `_PS`/`_CHx` family that also uses `expand_tidle`
        // (ISO9141_PS/ISO14230_PS).
        if let Err(err) = apply_j2534_params(
            &api,
            channel_id,
            j2534_proto_id,
            working_snapshot,
            Some(ComParamId(j2534_0404::DATA_RATE)),
        ) {
            let _ = api.disconnect(channel_id);
            drop(api);
            let last_error = self
                .logical_links
                .lock()
                .await
                .get(&handle)
                .and_then(|l| l.last_error.clone());
            return Err(map_native_error_for_link(
                "PassThruIoctl SET_CONFIG",
                &err,
                last_error,
            ));
        }

        // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15; reordered by
        // ADR-216 Decision item 9): `CONFIG_SAMPLE_RATE` is SET_CONFIG'd
        // AFTER the generic `apply_j2534_params` batch just above, not
        // immediately after `PassThruConnect` as originally implemented --
        // clause 10.3.3.2.3/.2.4's own semantics treat SAMPLES_PER_READING/
        // READINGS_PER_MSG (now forwardable through that generic batch via
        // ADR-216's own `comparam_id.rs` translation arm) as configuration of
        // an as-yet-unarmed subsystem, only armed once SAMPLE_RATE goes
        // nonzero -- applying the generic batch first, then arming the rate,
        // matches that intent directly instead of fighting it with an
        // artificial exception. Left in the original before-the-batch
        // position, staging either of those two ComParams together with the
        // sample rate ADR-178 already requires nonzero on every analog link
        // would make every such connect fail against a conforming device
        // (SAE J2534-2 clause 10.3.3.2.3/.2.4's own rate-must-be-zero
        // rejection), the moment either was added to the generic batch. A
        // device rejection (e.g. native `ERR_INVALID_IOCTL_VALUE`) still
        // fails the whole connect via the same rollback-and-map-native-error
        // path, surfaced through `map_native_error_for_link`/`pdu_error_for`
        // (error.rs) rather than a bespoke error path. `None` for every
        // non-analog connect skips this step entirely;
        // `rpc_connect_com_logical_link`'s own snapshot block (ADR-178)
        // already guarantees `rate` is nonzero whenever `Some`.
        // `CP_AnalogActiveChannels`/`CP_AnalogAveragingMethod` carry no such
        // ordering constraint (clause 10.3.3.2.1/.2.5) and are unaffected by
        // this reorder either way -- they are simply part of the generic
        // batch above like any other writable Analog Inputs ComParam.
        if let Some(rate) = analog_sample_rate
            && let Err(err) = api.set_config_u32(channel_id, j2534_0404::CONFIG_SAMPLE_RATE, rate)
        {
            let _ = api.disconnect(channel_id);
            drop(api);
            let last_error = self
                .logical_links
                .lock()
                .await
                .get(&handle)
                .and_then(|l| l.last_error.clone());
            return Err(map_native_error_for_link(
                "PassThruIoctl SET_CONFIG (CONFIG_SAMPLE_RATE)",
                &err,
                last_error,
            ));
        }

        // Install a pass-all filter so the adapter delivers received frames to the
        // service layer.  Without at least one filter J2534 adapters silently discard
        // all incoming frames.  Service-level filtering (UniqueRespIdTable matching)
        // is applied by poll_rx, so non-ISO15765 hardware filters are kept wide-open.
        // ISO15765 channels are handled by the caller instead (ADR-048).
        // ADR-157: gated on the base protocol id -- an ISO15765_PS channel
        // must not get an illegal pass-all filter either.
        // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15): also
        // skipped for an Analog Input channel -- unlike every other
        // protocol this service supports, clause 10 has NO filter concept
        // at all (`ioctl_start_msg_filter`'s own rejection for this
        // protocol), so there is no legitimate PASS_FILTER for this
        // service's own internal RX-delivery-enablement step to install
        // either; a real clause-10-conforming adapter delivers device-queued
        // readings to `PassThruReadMsgs` once armed via `CONFIG_SAMPLE_RATE`
        // alone, with no filter gating RX capture the way every other
        // protocol's hardware needs.
        //
        // SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a, PR #97
        // round 25, Codex review): also skipped for a TP2.0 channel -- ADR-188
        // §1 already documents this exclusion ("`install_pass_all_filter` is
        // skipped for this protocol ... clause 19's per-connection addressing
        // is the RX model, not a pass-all baseline"), but the exclusion was
        // never actually wired into this gate, so every TP2.0 physical-link
        // connect reached `PassThruStartMsgFilter` regardless. A conforming
        // TP2.0 adapter -- one that genuinely has no pass-all filter concept,
        // exactly like clause 10's Analog Inputs above -- would reject that
        // call and this connect would fail synchronously; the mock only
        // masked this because its own generic filter handler happens to
        // accept a TP2.0 protocol id it was never asked to reject.
        // `resources::is_tp2_0_protocol_id` is used directly (not a
        // `base_protocol_id` collapse) since `TP2_0_PS` already self-
        // identifies through `base_protocol_id`'s own identity fallback (see
        // that function's own doc comment).
        // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): also
        // skipped for an Ethernet_NDIS channel -- the same "no filter
        // concept at all" reasoning Analog Inputs' own exclusion above
        // documents (clause 24.2.5.5: `PassThruStartMsgFilter` always
        // returns `ERR_NOT_SUPPORTED`, so there is no legitimate PASS_FILTER
        // for this internal RX-delivery-enablement step to install either --
        // moot in practice since `rx_supported` gates the poll task's own RX
        // pass for this protocol regardless, but installing one here would
        // still fail the connect synchronously against a conforming
        // adapter/mock, exactly the TP2.0 regression this gate's own history
        // documents).
        if base_proto_id != j2534_0404::ISO15765
            && !resources::is_analog_in_protocol_id(base_proto_id)
            && !resources::is_tp2_0_protocol_id(base_proto_id)
            && base_proto_id != j2534_0404::PROTOCOL_ETHERNET_NDIS
            && let Err(err) =
                install_pass_all_filter(&api, channel_id, j2534_proto_id, connect_flags)
        {
            let _ = api.disconnect(channel_id);
            drop(api);
            let last_error = self
                .logical_links
                .lock()
                .await
                .get(&handle)
                .and_then(|l| l.last_error.clone());
            return Err(map_native_error_for_link(
                "PassThruStartMsgFilter",
                &err,
                last_error,
            ));
        }

        Ok(channel_id)
    }

    /// SAE J2534-2 clause 10 Analog Inputs (ADR-216 Decision item 6, amended
    /// by Codex review, PR #130, Finding 1): read-only half of the former
    /// `readback_analog_capability_params` -- performs the `GET_CONFIG`
    /// batch for `CONFIG_ACTIVE_CHANNELS`/`CONFIG_SAMPLE_RESOLUTION`/
    /// `CONFIG_INPUT_RANGE_LOW`/`CONFIG_INPUT_RANGE_HIGH`/
    /// `CONFIG_AVERAGING_METHOD` and the `[u32; 5]` destructuring only --
    /// touches neither `handle` nor `logical_links` at all. The result is
    /// threaded through `rpc_connect_com_logical_link` into
    /// `finalize_connected_link`, which performs the actual Working/Active
    /// write INSIDE its own connected-publishing critical section, rather
    /// than this function writing it itself in a separate, later
    /// acquisition of `logical_links` the way the original
    /// `readback_analog_capability_params` did. See
    /// `finalize_connected_link`'s own doc comment for the same-handle
    /// pipelined-`CoptUpdateparam` race this split closes.
    ///
    /// Best-effort, mirroring the `CP_Baudrate` write-back's own failure
    /// handling (`events.rs`, ADR-076): a native `GET_CONFIG` failure is
    /// logged and `None` is returned, so the caller leaves this CLL's
    /// ComParam sets unchanged rather than failing the connect itself.
    async fn read_analog_capability_params(
        &self,
        channel_id: ChannelId,
    ) -> Option<[(ComParamId, u32); 5]> {
        let result = {
            let api = self.api.lock().await;
            api.get_config(
                channel_id,
                &[
                    j2534_0404::CONFIG_ACTIVE_CHANNELS,
                    j2534_0404::CONFIG_SAMPLE_RESOLUTION,
                    j2534_0404::CONFIG_INPUT_RANGE_LOW,
                    j2534_0404::CONFIG_INPUT_RANGE_HIGH,
                    j2534_0404::CONFIG_AVERAGING_METHOD,
                ],
            )
        };
        let values = match result {
            Ok(values) => values,
            Err(err) => {
                warn!(
                    channel_id = channel_id.0,
                    %err,
                    "failed to read back CP_AnalogActiveChannels/CP_AnalogSampleResolution/\
                     CP_AnalogInputRangeLow/CP_AnalogInputRangeHigh/CP_AnalogAveragingMethod at \
                     ConnectComLogicalLink time; leaving the ComParam sets unchanged \
                     (best-effort readback only)"
                );
                return None;
            }
        };
        let Ok(
            [
                active_channels,
                sample_resolution,
                input_range_low,
                input_range_high,
                averaging_method,
            ],
        ): Result<[u32; 5], _> = values.try_into()
        else {
            warn!(
                channel_id = channel_id.0,
                "GET_CONFIG returned an unexpected number of values for the Analog Inputs \
                 capability readback; leaving the ComParam sets unchanged"
            );
            return None;
        };
        Some([
            (PARAM_ANALOG_ACTIVE_CHANNELS, active_channels),
            (PARAM_ANALOG_SAMPLE_RESOLUTION, sample_resolution),
            (PARAM_ANALOG_INPUT_RANGE_LOW, input_range_low),
            (PARAM_ANALOG_INPUT_RANGE_HIGH, input_range_high),
            (PARAM_ANALOG_AVERAGING_METHOD, averaging_method),
        ])
    }

    /// Spawns the poll task for a newly-created physical channel, resets module
    /// status to Ready (the adapter just responded to `PassThruConnect`), and
    /// registers the channel in `chans` with `ref_count = 1`.
    ///
    /// `connect_flags` is the value the channel was connected with (ADR-065);
    /// it is persisted on `SharedChannel` so `CLEAR_MSG_FILTERS` can rebuild the
    /// same per-ID-type `PASS_FILTER`s later.
    ///
    /// `applied_analog_sample_rate` (ADR-178) is recorded verbatim onto the
    /// new `SharedChannel` as `SharedChannel::applied_analog_sample_rate` --
    /// `Some(rate)` for a channel opened on one of the 32 `PROTOCOL_ANALOG_IN_x`
    /// ids, `None` for every other channel (including the UUDT-companion path,
    /// which is always CAN-family). Callers must pass the SAME
    /// Working-snapshot value already used to `SET_CONFIG` the hardware, not
    /// a fresh independent read, so the later join-mismatch check and
    /// `CoptUpdateparam` guard cannot race a concurrent `SetComParam`.
    ///
    /// `applied_analog_samples_per_reading`/`applied_analog_readings_per_msg`
    /// (ADR-216 Decision item 10 fix) mirror `applied_analog_sample_rate`
    /// exactly -- same gating, same connect-time snapshot requirement, same
    /// rationale -- and are recorded onto `SharedChannel::
    /// applied_analog_samples_per_reading`/`applied_analog_readings_per_msg`
    /// for the `CoptUpdateparam` guard's own value-comparison check on those
    /// two ComParams.
    ///
    /// `rx_supported` (ADR-194/Phase 16) seeds `ChannelPollCtx::rx_supported`
    /// -- `false` only for an Ethernet_NDIS channel (clause 24.2.5.2:
    /// `PassThruReadMsgs` always returns `ERR_NOT_SUPPORTED`), `true` for
    /// every other protocol. The poll task is still spawned uniformly either
    /// way (ADR-194 Decision, rejecting a protocol-based spawn skip) -- this
    /// flag only gates the shared RX pass's native read (`events.rs`).
    #[allow(clippy::too_many_arguments)]
    async fn spawn_new_shared_channel(
        &self,
        channel_key: ChannelKey,
        channel_id: ChannelId,
        connect_flags: u32,
        applied_analog_sample_rate: Option<u32>,
        applied_analog_samples_per_reading: Option<u32>,
        applied_analog_readings_per_msg: Option<u32>,
        rx_supported: bool,
        chans: &mut HashMap<ChannelKey, SharedChannel>,
    ) {
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel::<()>();
        let (tx_tx, tx_rx) = mpsc::unbounded_channel::<TxItem>();
        let executing_cop: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
        // Owned solely by the poll task -- CP_P3Func/P3Phys gap state has no
        // other reader, unlike `executing_cop` (read back via GetStatus), so
        // it is moved into `spawn_channel_poll_task` rather than stored on
        // `SharedChannel` (ADR-060).
        let last_func_tx: Arc<Mutex<Option<TxGapState>>> = Arc::new(Mutex::new(None));
        let last_phys_tx: Arc<Mutex<Option<TxGapState>>> = Arc::new(Mutex::new(None));
        // Per-shared-channel bus-idle clock for CP_TesterPresentSendType = 1
        // (ADR-083); armed to "now" at channel-open time, same as
        // `last_func_tx`/`last_phys_tx` starting empty.
        let last_bus_activity: Arc<Mutex<tokio::time::Instant>> =
            Arc::new(Mutex::new(tokio::time::Instant::now()));

        events::spawn_channel_poll_task(
            tx_rx,
            tx_tx.clone(),
            cancel_rx,
            self.shutdown.clone(),
            events::ChannelPollCtx {
                channel_id,
                primitives: Arc::clone(&self.primitives),
                api: Arc::clone(&self.api),
                logical_links: Arc::clone(&self.logical_links),
                drain_watermarks: Arc::clone(&self.drain_watermarks),
                subscriptions: Arc::clone(&self.subscriptions),
                module_state: Arc::clone(&self.module_state),
                module_event_buf: Arc::clone(&self.module_event_buf),
                system_event_buf: Arc::clone(&self.system_event_buf),
                executing_cop: Arc::clone(&executing_cop),
                last_func_tx,
                last_phys_tx,
                last_bus_activity,
                rx_supported,
                service: self.clone(),
            },
        );

        // ADR-131 Amendment (Codex review, PR #143, round 3 on the same
        // mechanism): this used to reset `module_state` to `PduModstReady`
        // here too ("a new physical channel means the VCI adapter
        // responded"), on the reasoning that a successful native
        // `PassThruConnect` is itself evidence of life. Removed: a hard
        // channel error on a DIFFERENT channel only tears down that
        // channel's CLLs, so `device_id` stays open and a fresh channel can
        // still legitimately connect on an otherwise-`NotAvail` module --
        // this reset let that success silently clear `NotAvail` without the
        // `ModuleDisconnect`-then-`ModuleConnect` sequence ADR-131 requires,
        // even bypassing `ModuleConnect`'s own now-sticky rejection entirely
        // (create a fresh CLL, connect it, then `ModuleConnect` would see
        // `Ready` again with no disconnect ever having happened). It was
        // also always redundant in the ordinary case: whenever this function
        // runs during a normal, never-`NotAvail` flow, `module_state` is
        // already `Ready` (either by default or via `ensure_open_device_inner`'s
        // fresh-open reset, which `CreateComLogicalLink`'s own
        // `ensure_open_device_for` call already ran before this point).
        // `module_state` is now reset to `Ready` in exactly one place:
        // `ensure_open_device_inner`'s fresh-`PassThruOpen` branch.

        // ADR-161: stamp a fresh occupancy epoch at channel creation, from
        // the same service-wide monotonic counter re-stamped on every
        // `ref_count` increment below.
        let occupancy_epoch = self.next_occupancy_epoch().await;

        chans.insert(
            channel_key,
            SharedChannel {
                channel_id,
                ref_count: 1,
                tx_queue: tx_tx,
                executing_cop,
                _poll_cancel: cancel_tx,
                // No pass-all filter is installed at connect time for any
                // protocol on this path (ISO15765: ADR-048 builds
                // point-to-point filters instead, and the zero-mask pass-all
                // fallback that used to be synced in later has been removed
                // entirely; non-ISO15765: pass-all is installed but not
                // tracked here, matching prior behaviour).
                connect_flags,
                dead: false,
                occupancy_epoch,
                leaked_repeat_message_ids: Vec::new(),
                leaked_periodic_message_ids: Vec::new(),
                applied_analog_sample_rate,
                applied_analog_samples_per_reading,
                applied_analog_readings_per_msg,
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
    }

    /// Writes the connected channel info into the CLL's state, promoting
    /// Working → Active when this CLL created the channel.
    ///
    /// Returns `Err(NotFound)` if the CLL was destroyed concurrently in the
    /// window between releasing `shared_channels` and re-acquiring
    /// `logical_links` — in that case, rolls back the `ref_count` bump (or the
    /// newly created channel) to prevent the physical channel and its poll task
    /// from leaking.  See ADR-012.
    ///
    /// ADR-123 Fix E: the caller acquires `shared_channels` first (ADR-080
    /// outermost) and holds it across both branches below -- the success
    /// branch's `recompute_lock_tx_suspensions` sweep and resume-wake sends
    /// need it to look up each resumed sibling's `tx_queue`, and the rollback
    /// branch reuses the same already-held guard instead of acquiring it
    /// fresh.
    ///
    /// **A2-7/ADR-129:** `chans` is the caller's own `shared_channels` guard,
    /// borrowed rather than re-locked here -- unlike every other field this
    /// function publishes, a new channel's install of this CLL's
    /// `pending_client_filters` (`rpc_connect_com_logical_link`) happens
    /// *before* the caller reaches this function, while `chans` was
    /// continuously held from `connect_new_physical_channel` onward. Re-
    /// acquiring `shared_channels` here (as this function used to) would
    /// reopen exactly the join-guard race this closes: a concurrent CLL's
    /// `client_filters`/`pending_client_filters` join check
    /// (`rpc_connect_com_logical_link`'s `filtered_by_another_cll` check)
    /// could then run in the gap between the filter install and this
    /// function's `channel_id`/`client_filters` publish, seeing neither.
    /// `installed_filters` is folded into `client_filters` (and
    /// `pending_client_filters` cleared) in the same `logical_links` critical
    /// section that publishes `channel_id` below, atomically with it.
    ///
    /// `join_epoch_rollback` is `Some((stamped_epoch, prev_epoch))` when the
    /// caller just joined an EXISTING channel (`is_new_channel == false`) and
    /// re-stamped its `occupancy_epoch`; `None` for the `is_new_channel` case,
    /// whose rollback removes the whole channel entry below and so has
    /// nothing to restore (ADR-161). See
    /// `restore_occupancy_epoch_on_rollback`'s doc comment for the
    /// compare-and-restore rule applied to it in the rollback branch below.
    ///
    /// `enforce_native_mixed_collision` (ADR-162 Decision 2): when `true`
    /// (computed by the caller the same way as its own connect-time
    /// self-check -- native-mixed mode and an unqualified link), this
    /// function's authoritative cross-CLL check runs FIRST, before anything
    /// below mutates `links`: this CLL's candidate `active_unique_resp_id_
    /// table` is compared, pooled with every already-published sibling CLL
    /// showing this same `channel_id`, for a `CP_CanRespUUDTId`/
    /// `CP_CanRespUSDTId` match-key collision. This is the only point that
    /// can see a sibling that joined concurrently -- the earlier connect-time
    /// self-check in `rpc_connect_com_logical_link` only ever sees this CLL's
    /// own table. A collision rolls back exactly like the pre-existing
    /// concurrent-destroy race below (same `rollback_channel_join` tail),
    /// returning a `PDU_ERR_FCT_FAILED` `Status` instead of that path's
    /// `unknown_handle_status`.
    ///
    /// `analog_connect_sync` (ADR-216 Decision items 6/10, amended by Codex
    /// review, PR #130, Finding 1): `(capability, samples_per_reading,
    /// readings_per_msg)`, all `None` for a non-analog connect. `capability`
    /// is the caller's already-completed `read_analog_capability_params`
    /// result (a live `GET_CONFIG` batch for `CP_AnalogActiveChannels`/
    /// `CP_AnalogSampleResolution`/`CP_AnalogInputRangeLow`/
    /// `CP_AnalogInputRangeHigh`/`CP_AnalogAveragingMethod`, `None` on a
    /// best-effort read failure); `samples_per_reading`/`readings_per_msg`
    /// are `CP_AnalogSamplesPerReading`/`CP_AnalogReadingsPerMsg`, sourced by
    /// the caller from the Working-snapshot locals for a fresh connect or
    /// from `SharedChannel::applied_analog_samples_per_reading`/
    /// `applied_analog_readings_per_msg` for a join. This function writes
    /// every present key into BOTH this CLL's Working and Active
    /// `ComParamSet` alike, INSIDE this same critical section, immediately
    /// after the `is_new_channel` branch above has already set `link.active
    /// = working_snapshot` (so these values win on top of it for a fresh
    /// connect) but unconditionally regardless of `is_new_channel` (so a
    /// join gets exactly these keys written into its otherwise-still-default
    /// Active, matching the existing "Active stays default until
    /// CoptUpdateparam" comment for every OTHER ComParam).
    ///
    /// This used to run as two separate calls
    /// (`readback_analog_capability_params`/
    /// `sync_analog_batching_params_from_applied`) AFTER this function
    /// already published `connected = true`/`channel_key` and released
    /// `logical_links` -- a client that already holds `cll_handle` (returned
    /// by the earlier `CreateComLogicalLink`) and pipelines
    /// `ConnectComLogicalLink` immediately followed by
    /// `StartComPrimitive(CoptUpdateparam)` on the same handle, without
    /// waiting for the connect response (gRPC gives no ordering guarantee
    /// between the two independent RPC calls), could have its own
    /// `CoptUpdateparam` promote this CLL's Active to a newer value, only for
    /// the still-in-flight capability readback's stale `GET_CONFIG` snapshot
    /// (read earlier, before the race) to then overwrite BOTH Working and
    /// Active back to the stale value -- clobbering the client's own
    /// just-issued intent even though hardware already reflects the newer
    /// value. Moving the write inside this critical section closes that
    /// window: `link.active = working_snapshot` (fresh connect) and this
    /// write both complete before `connected`/`channel_key` become visible
    /// to any other call, so a pipelined `CoptUpdateparam` on this handle can
    /// never race either of them.
    #[allow(clippy::too_many_arguments)]
    async fn finalize_connected_link(
        &self,
        handle: u32,
        channel_id: ChannelId,
        channel_key: ChannelKey,
        is_new_channel: bool,
        working_snapshot: ComParamSet,
        installed_filters: HashMap<u32, Vec<MessageFilterId>>,
        chans: &mut HashMap<ChannelKey, SharedChannel>,
        join_epoch_rollback: Option<(u64, u64)>,
        enforce_native_mixed_collision: bool,
        analog_connect_sync: AnalogConnectSync,
    ) -> Result<(), Status> {
        let mut links = self.logical_links.lock().await;

        // ADR-162 Decision 2: authoritative cross-CLL collision check, run
        // before this critical section mutates anything below. Immutable
        // scan only -- this CLL's own candidate table plus every OTHER link
        // already showing `channel_id` (a sibling that finished its own
        // `finalize_connected_link` call and published `channel_id` first).
        if enforce_native_mixed_collision && let Some(candidate) = links.get(&handle) {
            let candidate_table = candidate.active_unique_resp_id_table.clone();
            let sibling_tables: Vec<Vec<EcuUniqueRespEntry>> = links
                .iter()
                .filter(|&(&h, l)| h != handle && l.channel_id == Some(channel_id))
                .map(|(_, l)| l.active_unique_resp_id_table.clone())
                .collect();
            let mut tables: Vec<&[EcuUniqueRespEntry]> = vec![candidate_table.as_slice()];
            tables.extend(sibling_tables.iter().map(Vec::as_slice));
            if let Some(collision) = find_native_mixed_uudt_usdt_collision(&tables) {
                let last_error = links.get(&handle).and_then(|l| l.last_error.clone());
                drop(links);
                self.rollback_channel_join(chans, channel_key, join_epoch_rollback)
                    .await;
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    format!(
                        "PDU_ERR_FCT_FAILED: UniqueRespIdTable rejected -- CAN address \
                         {collision:?} is configured as both a flow-control-eligible \
                         CP_CanRespUSDTId and a CP_CanRespUUDTId on this shared physical \
                         channel (this CLL's own table, or a sibling ComLogicalLink's table \
                         already connected on it); under native-mixed CAN mode SAE J2534-2 \
                         clause 8's CAN_MIXED_FORMAT_ON semantics always route a match on that \
                         address to the FLOW_CONTROL_FILTER, so the UUDT interpretation can \
                         never be observed"
                    ),
                    PduError::PduErrFctFailed,
                    last_error,
                ));
            }
        }

        let connected = if let Some(link) = links.get_mut(&handle) {
            link.channel_id = Some(channel_id);
            link.channel_key = Some(channel_key);
            link.connected = true;
            // Stamp a freshly-allocated generation on every finalized
            // connect, including a reconnect of the same cll_handle onto
            // the same physical channel (ADR-086). `next_connect_generation`
            // takes its own independent Mutex, unrelated to `logical_links`,
            // so there is no lock-ordering hazard in awaiting it here.
            link.connect_generation = self.next_connect_generation().await;
            // ADR-180 Decision 17 (Codex review, PR #72 round 13): a fresh
            // connect_generation must never inherit a prior generation's
            // SAE J1939 claim state either -- `cancel_j1939_claims_for_cll`
            // (disconnect teardown) only clears the SHARED
            // `SharedChannel::j1939_claims` routing entry, since it takes no
            // `logical_links` reference at all; the per-CLL LOCAL
            // `j1939_claimed_address`/`j1939_claim_cursor` fields on THIS
            // struct otherwise survive a disconnect untouched. Without this
            // reset, a same-handle reconnect leaves `j1939_claimed_address`
            // pointing at an address the adapter is no longer defending (the
            // disconnect's own cancel already relinquished it natively),
            // while every "am I still unclaimed" gate that reads it
            // (`j1939_negotiated_unclaimed`, Decisions 13/14/16) wrongly
            // reports "claimed" until a fresh `CoptStartcomm` overwrites it --
            // e.g. `ioctl_start_repeat_message` would then permit an
            // autonomous repeat slot framed with the stale, now-undefended
            // Active `NODE_ADDRESS`. Reset unconditionally: a brand-new CLL's
            // very first connect already has both fields at their
            // `LogicalLinkState` default (`None`/`0`), so this is a no-op for
            // that case and the actual fix only for a reconnect.
            link.j1939_claimed_address = None;
            link.j1939_claim_cursor = 0;
            // Codex review finding (PR #97): same reconnect-only reasoning as
            // Decision 17 just above, for SAE J2534-2 clause 19 TP2.0's own
            // per-CLL connection state -- `Tp20Connection`'s own doc comment
            // ("NOT cleared on disconnect -- reset to `None` at the next
            // `ConnectComLogicalLink`'s finalization") already promised this
            // reset, but nothing actually implemented it until now. Without
            // it, a same-handle reconnect (including one following a hard
            // channel error, which does not itself clear `tp20_connection`
            // either, matching this file's own J1939 precedent) would leave
            // the PRIOR generation's `Established` phase and device-assigned
            // TX-ID/RX-ID visible to a `CoptSendrecv`/`PDU_IOCTL_START_
            // REPEAT_MESSAGE` issued before a fresh `CoptStartcomm` re-runs
            // the clause 19 connection-request lifecycle for this new
            // generation -- both `build_tx_message`'s TP2.0 arm and RX
            // routing (`build_cll_rx_entries`) key directly off this field,
            // so a stale `Some(Established)` would frame/route against a
            // native TX-ID/RX-ID this new physical channel never actually
            // negotiated. No-op for a first connect (`LogicalLinkState::
            // default` already starts this field `None`), the actual fix
            // only for a reconnect.
            link.tp20_connection = None;
            // ADR-180 Decision 18: same reconnect-only reasoning as
            // Decision 17 just above -- a fresh `connect_generation` must
            // not inherit a prior generation's negotiation posture either,
            // or a CLL that structurally opted OUT of negotiation before
            // this reconnect would spuriously start failing sends gated by
            // `j1939_negotiated_unclaimed_for`'s `Engaged` arm (or, before
            // its correction, spuriously stay blocked by a stale `OptedOut`
            // reading as "still negotiated"). No-op for a first connect
            // (`LogicalLinkState::default` already starts this field
            // `Undecided`), the actual fix only for a reconnect that
            // inherits a stale `Engaged`/`OptedOut` from before.
            link.j1939_negotiation_posture = J1939NegotiationPosture::Undecided;
            // ADR-147: a fresh session must never inherit a prior session's
            // `CP_SuspendQueueOnError` error-suspension. Reset directly (not
            // via `clear_error_suspension`) -- no `error_clear_seq` bump
            // needed here: the existing `connect_generation` check at apply
            // time already discards any old-session pass's classification on
            // its own, and since the third amendment (capture-at-fold
            // sequencing) restored `clear_error_suspension`'s unconditional
            // bump, every teardown-site clear (`handle_channel_hard_error`
            // included) already bumps `error_clear_seq` regardless of the
            // flag's prior state, which alone invalidates any pre-offline
            // Suspend fold's captured `suspend_seq` at apply time (a
            // pre-offline Positive fold is separately closed by its own
            // batch-read anchor, `error_set_seq`/`set_seq_at_read`, ADR-147
            // fifth amendment). This reset is therefore hygiene, not a race
            // fix on its own: a fresh session must never inherit a prior
            // session's error-suspension state, full stop, independent of
            // whichever mechanism happens to also block a stale pass from
            // applying.
            link.tx_suspended_by_error = false;
            if is_new_channel {
                // First CLL on this channel: Working → Active (hardware reflects the Working set).
                link.active = working_snapshot;
            }
            // SAE J2534-2 clause 10 Analog Inputs (ADR-216 Decision items
            // 6/10, amended by Codex review, PR #130, Finding 1): write the
            // analog connect-time sync into BOTH Working and Active here,
            // inside this same critical section -- after the `is_new_channel`
            // branch above (so these values win on top of a fresh connect's
            // `link.active = working_snapshot`) but unconditionally,
            // regardless of `is_new_channel` (so a join also gets these
            // specific keys, unlike every other ComParam). See this
            // function's own doc comment for `analog_connect_sync`'s shape
            // and the same-handle pipelined-`CoptUpdateparam` race this
            // closes. A `None` capability/batching value (non-analog
            // connect, or a best-effort `GET_CONFIG` read failure) writes
            // nothing for that part.
            let (analog_capability, analog_samples_per_reading, analog_readings_per_msg) =
                analog_connect_sync;
            if let Some(capability) = analog_capability {
                for (param_id, value) in capability {
                    link.working.unum32.insert(param_id, value);
                    link.active.unum32.insert(param_id, value);
                }
            }
            if let Some(value) = analog_samples_per_reading {
                link.working
                    .unum32
                    .insert(PARAM_ANALOG_SAMPLES_PER_READING, value);
                link.active
                    .unum32
                    .insert(PARAM_ANALOG_SAMPLES_PER_READING, value);
            }
            if let Some(value) = analog_readings_per_msg {
                link.working
                    .unum32
                    .insert(PARAM_ANALOG_READINGS_PER_MSG, value);
                link.active
                    .unum32
                    .insert(PARAM_ANALOG_READINGS_PER_MSG, value);
            }
            // Joining CLLs: Active stays default until the CLL issues CoptUpdateparam.
            // A fresh connect_generation must never inherit a partial
            // software-ISO-TP reassembly from a previous generation on
            // this cll_handle: without this, a stale FirstFrame captured
            // before a disconnect could be completed by a
            // ConsecutiveFrame received after a reconnect, mixing bytes
            // from two different sessions (ADR-086 amendment). Cleared
            // *inside* the `logical_links` lock so the clear is atomic
            // with the new `connect_generation` becoming visible --
            // `build_cll_rx_entries` (events.rs) also takes
            // `logical_links` to build its RX snapshot, so any poll-task
            // pass that can observe this generation is built strictly
            // after the clear completes; a post-reconnect frame can
            // never be wiped by it. Lock order `logical_links` ->
            // `isotp_rx` is safe: the only other `isotp_rx` lock sites
            // (events.rs's FirstFrame/ConsecutiveFrame reassembly paths)
            // acquire no other lock while holding it.
            link.isotp_rx.lock().await.clear();
            // A2-7/ADR-129: `installed_filters` is non-empty only when the
            // caller's new-channel branch just installed this CLL's
            // pre-connect `pending_client_filters` onto hardware -- a join
            // onto an existing channel never reaches here with pending
            // filters non-empty (the caller rejects that combination up
            // front, ADR-082), so this is an empty-map no-op in that case.
            for (filter_number, filter_ids) in installed_filters {
                link.client_filters.insert(filter_number, filter_ids);
            }
            link.pending_client_filters.clear();
            true
        } else {
            false
        };
        if connected {
            // ADR-123 Fix E: sweep in the SAME critical section that just
            // published `connected`/`channel_key` above, not a separate one
            // re-acquired by the caller afterward -- see the invariant this
            // closes in ADR-123 §3 (same shape as Fix A's siphon/grant
            // serialization). This CLL may itself need to inherit
            // `tx_suspended_by_lock` from a sibling already holding
            // `LOCK_PHYSICAL_TX_QUEUE` on this resource (spec line 1810:
            // a newly created ComLogicalLink starts with its ComPrimitive
            // queue suspended), and this CLL's join can
            // also flip another CLL's fallback-vs-real-`channel_key`
            // resource match (see `recompute_lock_tx_suspensions`'s doc
            // comment), so recompute for everyone.
            let resumed = recompute_lock_tx_suspensions(&mut links);
            let wake_targets: Vec<(u32, mpsc::UnboundedSender<TxItem>)> = resumed
                .into_iter()
                .filter_map(|h| {
                    links
                        .get(&h)
                        .and_then(|l| l.channel_key)
                        .and_then(|ck| chans.get(&ck).map(|sc| (h, sc.tx_queue.clone())))
                })
                .collect();
            drop(links);
            for (h, tx_queue) in wake_targets {
                let _ = tx_queue.send(TxItem::ResumeWake { cll_handle: h });
            }
            return Ok(());
        }
        drop(links);

        // ADR-012 rollback path: the CLL was destroyed concurrently in the
        // window between the caller acquiring `shared_channels` and
        // `finalize_connected_link` re-acquiring `logical_links` above. Roll
        // back the `ref_count` bump (or the newly created channel) so the
        // physical channel and its poll task do not leak. Any filters this
        // connect already installed from `pending_client_filters` (only
        // possible when `is_new_channel`) are torn down for free here too:
        // a fresh channel's `ref_count` is still exactly 1 (nothing else
        // could have joined it -- `chans` has been held continuously since
        // before it was created), so the decrement below always reaches 0
        // and `PassThruDisconnect` tears down every hardware filter on the
        // channel along with it.
        self.rollback_channel_join(chans, channel_key, join_epoch_rollback)
            .await;
        Err(unknown_handle_status(format!(
            "cll_handle {handle} was destroyed while ConnectComLogicalLink was in progress"
        )))
    }

    /// Shared rollback tail for `finalize_connected_link`'s two reject paths
    /// (ADR-012/ADR-161's concurrent-destroy race, and ADR-162 Decision 2's
    /// native-mixed collision reject) and, since ADR-161's dedicated
    /// follow-up gave it the same continuously-held-guard shape,
    /// `ensure_uudt_companion_channel`'s own concurrent-destroy reject path
    /// too: undoes the `ref_count` bump (or the newly created channel) the
    /// caller already applied before reaching this function, so the physical
    /// channel and its poll task do not leak. Reuses the caller's `chans`
    /// guard instead of acquiring one fresh.
    ///
    /// ADR-161 rollback sub-rule: restores the pre-join `occupancy_epoch`
    /// (compare-and-restore, not blind) before the decrement below -- see
    /// `restore_occupancy_epoch_on_rollback`'s doc comment.
    async fn rollback_channel_join(
        &self,
        chans: &mut HashMap<ChannelKey, SharedChannel>,
        channel_key: ChannelKey,
        join_epoch_rollback: Option<(u64, u64)>,
    ) {
        if let Some(sc) = chans.get_mut(&channel_key) {
            Self::restore_occupancy_epoch_on_rollback(sc, join_epoch_rollback);
            sc.ref_count -= 1;
            if sc.ref_count == 0
                && let Some(sc) = chans.remove(&channel_key)
            {
                // A2-7/ADR-129: unlike before this function's `chans` became
                // a borrow of the caller's own guard, there is no longer a
                // local lock to drop early here -- the caller releases
                // `shared_channels` after this call returns. `self.api`/
                // `self.drain_watermarks` remain innermost either way, so
                // acquiring them here is still lock-order safe (ADR-080).
                // ADR-101 Decision §E: hygiene, not correctness -- see the
                // sibling teardown sites' identical comment for why.
                self.drain_watermarks.lock().await.remove(&sc.channel_id);
                let api = self.api.lock().await;
                let _ = api.disconnect(sc.channel_id);
            }
        }
    }

    /// Calls `PassThruConnect` with the baud rate from the Working param set,
    /// applies all remaining Working J2534 params to the hardware, then copies
    /// Working → Active so the Active set reflects what is on the hardware.
    ///
    /// Also promotes `working_unique_resp_id_table` → `active_unique_resp_id_table`
    /// for THIS CLL -- unlike the ComParamSet promotion above, which happens
    /// only for the channel-creator, the table promotes at every connecting
    /// CLL's own Connect, since ISO15765 `FLOW_CONTROL_FILTER`s are per-CLL,
    /// not per-channel (ADR-039, ADR-068).
    ///
    /// The baud rate must have been set via `SetComParam(DATA_RATE, …)` before
    /// this is called; a rate of 0 will be rejected by most J2534 adapters.
    pub(super) async fn rpc_connect_com_logical_link(
        &self,
        request: Request<vci_service_interface::ConnectComLogicalLinkRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;

        // Claim the "Connect in flight" token before anything else runs
        // (ADR-126 round 2): this closes the window between this RPC being
        // accepted and `finalize_connected_link` publishing `connected =
        // true`, during which `PDU_IOCTL_SET_EVENT_QUEUE_PROPERTIES`'s old
        // `connected`-only guard would otherwise let a concurrent call
        // through even though PDUConnect has already been called (ISO
        // 22900-2 §9.5.16 gates on call time, not completion time). Also
        // absorbs the idempotent already-connected no-op check, so this is
        // the only place that needs the early `connected` read.
        //
        // `_connect_token` is an `Arc<()>` whose only `Weak` is stashed on
        // `link.connect_in_flight`; it must stay alive for the rest of this
        // function (bound here, not in an inner block) so `pdu_connect_begun`
        // sees it as in-flight until this RPC returns via ANY path --
        // success, `?`, or the future being dropped/cancelled by tonic.
        let _connect_token = {
            let mut links = self.logical_links.lock().await;
            let link = links
                .get_mut(&handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
            // ADR-134 (Codex review, PR #149): this fast path is deliberately
            // NOT gated on `module_state.status` -- it is reachable only by a
            // CLL whose own channel was never hit by a hard error.
            // `events::handle_channel_hard_error` clears `connected = false`
            // (and `channel_id`) for every CLL on the channel that actually
            // failed, so a CLL reaching this branch with `connected == true`
            // is, by construction, a genuinely-unaffected sibling still
            // communicating on a healthy channel (ADR-131's own per-channel
            // isolation design: a hard error on one channel does not disturb
            // siblings). Returning success here is a true statement about
            // THIS CLL, not a stale/unrevalidated claim about the module the
            // way `ModuleConnect`'s original bug was -- there is no native
            // call to gate and nothing new to open or join, so this repeat
            // Connect no-op is unaffected by whether some OTHER channel's
            // module_state is `NotAvail`.
            if link.connected {
                return Ok(Self::empty_response());
            }
            if link.connect_in_flight.upgrade().is_some() {
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "ConnectComLogicalLink is already in progress for this ComLogicalLink",
                    PduError::PduErrCllConnected,
                    link.last_error.clone(),
                ));
            }
            let token = Arc::new(());
            link.connect_in_flight = Arc::downgrade(&token);
            token
        };

        // SAE_J1850 VPW/PWM auto-detect (ADR-070): a no-op for every protocol
        // except the two `SAE_J1850` bus-agnostic ones, and for those, a
        // no-op once the module-wide cache already holds a flavor. Must run
        // before the block below reads `hw_protocol_id`/`working` so a PWM
        // win's overrides are already in place when this CLL's own snapshot
        // is taken, and before the channel is registered so co-tenant CLLs
        // are unaffected either way.
        self.autodetect_sae_j1850_flavor(handle).await;

        // SAE J2534-2 clause 21 CAN FD connect-time substitution (ADR-158):
        // must also run before the snapshot block below reads
        // `link.hw_protocol_id`/`base_hw_protocol_override`, mirroring the
        // J1850 autodetect call just above -- see `apply_fd_mode`'s own doc
        // comment.
        self.apply_fd_mode(handle).await?;

        let (
            protocol,
            j2534_proto_id,
            base_proto_id,
            baud_rate,
            working_snapshot,
            has_uudt_ids,
            connect_flags,
            pin_select,
            link_pin_select,
            link_channel_index,
            fd_data_phase_rate,
            analog_sample_rate,
            analog_samples_per_reading,
            analog_readings_per_msg,
        ) = {
            let mut links = self.logical_links.lock().await;
            let link = links
                .get_mut(&handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
            let j2534_proto_id = link.hw_protocol_id;
            // Promote the UniqueRespIdTable for THIS CLL now, before
            // connect_flags/UUDT below read it -- in-memory only, no hardware
            // I/O yet, so it is harmless even if the physical connect attempt
            // below subsequently fails (ADR-068).
            link.active_unique_resp_id_table = link.working_unique_resp_id_table.clone();
            // Decided once here, from the CLL's Working set and just-promoted
            // Active UniqueRespIdTable: the shared-channel creator-decides rule
            // (ADR-044) means a table set on this CLL after connect does not
            // retroactively change the flags a joining CLL already connected with,
            // and vice versa (ADR-065).
            //
            // ADR-157 Plane B: `connect_flags`'s match is a protocol-family
            // gate (CAN width, ISO15765 vs. K-line), not a hardware-identity
            // use, so it must see the base protocol id even for a `_PS`
            // link -- found during this fix's independent sweep, not in
            // ADR-157's own enumerated site list, but the same
            // classification applies. `j2534_proto_id` itself (passed to
            // the real `PassThruConnect` below) stays the raw `_PS` id.
            let connect_flags = connect_flags(
                link.base_hw_protocol_id(),
                &link.working,
                &link.active_unique_resp_id_table,
                // ADR-198 Phase 2: consulted only by the ISO9141/ISO14230
                // arm -- see `connect_flags`'s own doc comment.
                link.raw_mode,
                link.checksum_mode,
            );
            // ADR-158: an FD_CAN_PS link (already substituted by
            // `apply_fd_mode`, which ran before this critical section)
            // always needs an explicit CONFIG_J1962_PINS pin assignment,
            // unlike Classic CAN's default (pin-preassigned) connect --
            // clause 21 has no such default, only `_PS`/`_CHx` variants. A
            // genuine Pin Selection value (`link.pin_select`) wins if
            // present; otherwise the base protocol's own default packed
            // pins substitute (`resources::default_pin_select_for_base`),
            // flowing into the physical connect parameters below (the
            // `channel_key`/`NewPhysicalChannelParams.pin_select` this
            // block produces) only -- never into `link.pin_select` itself
            // (that would make a later plain-CAN reconnect on this same
            // link incorrectly think it still needs to issue
            // SET_CONFIG(CONFIG_J1962_PINS); see `apply_fd_mode`'s own doc
            // comment for the symmetric revert). ADR-213 Decision item 3:
            // this synthesis is additionally gated on `channel_index.is_none()`
            // -- a `_CHx`-connected FD channel is already vendor-pin-
            // preassigned per clause 21.3.2.5.1/22.3.2.6.1, so issuing
            // SET_CONFIG(CONFIG_J1962_PINS) on it would be a spec violation
            // (`is_fd_protocol_id` now also recognizes an FD `_CHx` id, so
            // without this extra gate this synthesis would incorrectly fire
            // for one).
            let physical_pin_select = link.pin_select.or_else(|| {
                (resources::is_fd_protocol_id(j2534_proto_id) && link.channel_index.is_none())
                    .then(|| resources::default_pin_select_for_base(link.base_hw_protocol_id()))
                    .flatten()
            });
            // ADR-158: the CAN FD data-phase rate `connect_new_physical_channel`
            // must SET_CONFIG(CONFIG_FD_CAN_DATA_PHASE_RATE) before
            // CONFIG_J1962_PINS (clause 21.3.2.5.1) -- `None` skips that step
            // entirely for every non-FD connect.
            let fd_data_phase_rate = resources::is_fd_protocol_id(j2534_proto_id).then(|| {
                let canfd_baudrate = link
                    .working
                    .unum32
                    .get(&PARAM_CANFD_BAUDRATE)
                    .copied()
                    .unwrap_or(0);
                if canfd_baudrate != 0 {
                    canfd_baudrate
                } else {
                    link.working.baud_rate()
                }
            });
            // SAE J2534-2 clause 10 Analog Inputs (ADR-178, revising
            // ADR-177/Phase 15): required-nonzero validation moves from
            // `CreateComLogicalLink` time to here -- `CP_AnalogSampleRate`
            // (`PARAM_ANALOG_SAMPLE_RATE`) is now staged via `SetComParam`
            // rather than a request field, so it must be read from this
            // CLL's Working ComParam set and validated right here, still
            // holding the `logical_links` lock, rather than trusted as
            // already-validated. The inverse rejection (this ComParam set
            // on a non-analog link) needs no separate check here -- it is
            // enforced structurally by `comparam_support::is_param_allowed`'s
            // ANALOG_IN allowlist at `SetComParam` time (ADR-178 line
            // 146-147). `None` skips `connect_new_physical_channel`'s
            // `CONFIG_SAMPLE_RATE` SET_CONFIG step entirely for every
            // non-analog connect, mirroring `fd_data_phase_rate`'s own shape
            // just above. This is also the ONE snapshot read threaded
            // through to `SharedChannel::applied_analog_sample_rate` below
            // (ADR-178 line 156) -- never re-read independently later.
            let is_analog_resource = resources::is_analog_in_protocol_id(j2534_proto_id);
            let analog_sample_rate_value = link
                .working
                .unum32
                .get(&PARAM_ANALOG_SAMPLE_RATE)
                .copied()
                .unwrap_or(0);
            if is_analog_resource && analog_sample_rate_value == 0 {
                return Err(Status::invalid_argument(
                    "CP_AnalogSampleRate is required and must be staged nonzero via \
                     SetComParam before connecting to a SAE J2534-2 clause 10 Analog Input \
                     resource (ADR-178) -- clause 10.3.3.2.2's own zero default means the \
                     acquisition subsystem is disabled",
                ));
            }
            let analog_sample_rate = is_analog_resource.then_some(analog_sample_rate_value);
            // ADR-216 Decision item 10 fix: the same connect-time snapshot
            // read as `analog_sample_rate_value` just above, for the two
            // ComParams the `CoptUpdateparam` guard (`rpc_primitive.rs`)
            // needs an "applied" value to compare against -- see
            // `SharedChannel::applied_analog_samples_per_reading`'s own doc
            // comment for the full reasoning. `1` mirrors
            // `comparam_defaults.rs`'s `analog_in()` own seeded default for
            // both ids.
            let analog_samples_per_reading = is_analog_resource.then(|| {
                link.working
                    .unum32
                    .get(&PARAM_ANALOG_SAMPLES_PER_READING)
                    .copied()
                    .unwrap_or(1)
            });
            let analog_readings_per_msg = is_analog_resource.then(|| {
                link.working
                    .unum32
                    .get(&PARAM_ANALOG_READINGS_PER_MSG)
                    .copied()
                    .unwrap_or(1)
            });
            (
                link.protocol,
                j2534_proto_id,
                // ADR-157 Plane B (Correction, Codex review, PR #28): the
                // base protocol id for every family/behavior decision below
                // in this function (the ISO15765 FC-filter gate and the CAN
                // dual-channel-mode auto-probe gate) -- `j2534_proto_id`
                // itself stays the raw `_PS` id for `ChannelKey`/
                // `PassThruConnect`/message building (Plane A/C). Read
                // directly from `link.base_hw_protocol_id()` while `link` is
                // still in scope, rather than reconstructed afterward from
                // `protocol.j2534_protocol_id()` -- that reconstruction
                // assumed `protocol` (this CLL's stored `ChannelProtocol`)
                // is always the pre-existing base protocol whenever
                // `pin_select` is set, which held before a caller could name
                // a `_PS` id directly via `protocol_id` (5cd4fec): for that
                // case `protocol` is itself `ChannelProtocol::from_raw` of
                // the raw `_PS` id, so `j2534_protocol_id()`'s catch-all
                // pass-through returned the `_PS` id unchanged instead of
                // normalizing it, silently skipping the ISO15765
                // FC-filter-installation branch below for such a link.
                link.base_hw_protocol_id(),
                link.working.baud_rate(),
                link.working.clone(),
                link.active_unique_resp_id_table
                    .iter()
                    .any(|e| uudt_resp_id(e).is_some()),
                connect_flags,
                // ADR-156 Decision 2 (amended, ADR-158): `0` for every
                // non-`_PS`/non-FD link (the overwhelming majority),
                // reproducing the pre-Phase-2a `ChannelKey` 2-tuple's exact
                // sharing behavior for them; `physical_pin_select`'s value
                // otherwise (a genuine Pin Selection, or an FD link's
                // default packed pins).
                physical_pin_select.unwrap_or(0),
                // Distinct from the field above: `None` (not `Some(0)`)
                // means "no pins `SET_CONFIG` to issue at all" for
                // `connect_new_physical_channel` -- a real `_PS`/FD pin
                // selection is never actually `0x00000000` (every packed
                // pin number is nonzero), so this is never ambiguous with a
                // genuine `_PS`/FD link.
                physical_pin_select,
                // ADR-156 Decision 3/Phase 2b: this link's Additional
                // Channels index, if any -- used for the capacity precheck
                // below and to widen the `_PS`-only qualifier skips further
                // down to "any qualifier" (ADR-156 Decision 3 addendum).
                link.channel_index,
                fd_data_phase_rate,
                analog_sample_rate,
                analog_samples_per_reading,
                analog_readings_per_msg,
            )
        };

        // SAE J2534-2 clause 8 Mixed Format Frames on a CAN Network
        // (ADR-160/Phase 3c; split into three locals by ADR-217): computed
        // here, after `logical_links` was released above and before
        // `device_id` is acquired below -- `effective_can_channel_mode()`
        // must never be called while holding `api` or `device_id` (see
        // `install_point_to_point_fc_filters`'s identical AB-BA-deadlock
        // note for `dual_channel`). `effective_can_mode` is read once and
        // reused for all three locals below, rather than calling
        // `effective_can_channel_mode()` again, since it is an async call
        // that locks `resolved_can_channel_mode`.
        //
        // - `native_mixed_applies`: `true` iff this link's primary channel
        //   is ISO15765-family, not FD-substituted (ADR-159's own fallback
        //   stays in place for `FD_ISO15765_PS`), the module's effective CAN
        //   channel mode is either native-mixed sub-mode
        //   (`is_native_mixed_family()`), and the link is unqualified (no
        //   pin selection, no `_CHx` Additional Channels index) -- folding
        //   in, once, the qualified-link exclusion that ADR-160's Correction
        //   previously required each of three downstream call sites to
        //   append independently (the ADR-162 fail-fast self-check just
        //   below, the `NewPhysicalChannelParams` construction, and the
        //   `finalize_connected_link` call site further down).
        // - `native_mixed_set_config_value`: `None` when `!native_mixed_
        //   applies`; otherwise `Some(CAN_MIXED_FORMAT_ON)` or
        //   `Some(CAN_MIXED_FORMAT_ALL_FRAMES)` depending on which
        //   native-mixed variant `effective_can_mode` resolved to. Feeds
        //   `NewPhysicalChannelParams`'s `native_mixed_format` field.
        //   Actually applied only when `connect_new_physical_channel` runs
        //   below (a genuinely new physical channel) -- a CLL joining an
        //   already-open shared channel never re-issues the connect-time
        //   SET_CONFIG.
        // - `native_mixed_collision_enforced`: `native_mixed_applies` AND
        //   specifically `CanChannelMode::NativeMixed` (not the family
        //   check) -- the ADR-162 UUDT/USDT collision checks stay `ON`-only
        //   (ADR-217 Decision item 3): under `ALL_FRAMES` the same overlap
        //   is servable, not a hazard.
        let effective_can_mode = self.effective_can_channel_mode().await;
        let native_mixed_applies = base_proto_id == j2534_0404::ISO15765
            && !resources::is_fd_protocol_id(j2534_proto_id)
            && effective_can_mode.is_native_mixed_family()
            && link_pin_select.is_none()
            && link_channel_index.is_none();
        let native_mixed_set_config_value = if !native_mixed_applies {
            None
        } else if effective_can_mode == CanChannelMode::NativeMixed {
            Some(CAN_MIXED_FORMAT_ON)
        } else {
            Some(CAN_MIXED_FORMAT_ALL_FRAMES)
        };
        let native_mixed_collision_enforced =
            native_mixed_applies && effective_can_mode == CanChannelMode::NativeMixed;

        // ADR-162 Decision 2, fail-fast self-check: rejects THIS CLL's own
        // `UniqueRespIdTable` if it self-collides (a UUDT match key equal to
        // one of its own flow-control-eligible USDT match keys) under
        // native-mixed mode, before any native/hardware call runs -- a plain
        // early return needs no rollback. Mirrors `install_point_to_point_fc_
        // filters`'s own `!qualified` gate: a qualified (pin-selected/`_CHx`)
        // link never gets the native-mixed `PASS_FILTER`, so it is never
        // collision-checked either. This alone fully covers a fresh physical
        // channel (no sibling exists yet to collide against); the
        // cross-CLL case is caught by `finalize_connected_link` below, which
        // can see already-connected siblings on a shared channel. ADR-217:
        // `native_mixed_collision_enforced` already folds in the qualified-
        // link exclusion, and stays `NativeMixed`-only -- unaffected by
        // `NativeMixedAllFrames`, under which this same overlap is
        // servable.
        if native_mixed_collision_enforced {
            let (candidate_table, last_error) = {
                let links = self.logical_links.lock().await;
                let Some(link) = links.get(&handle) else {
                    return Err(unknown_handle_status(format!(
                        "cll_handle {handle} was destroyed while ConnectComLogicalLink was in \
                         progress"
                    )));
                };
                (
                    link.active_unique_resp_id_table.clone(),
                    link.last_error.clone(),
                )
            };
            if let Some(collision) =
                find_native_mixed_uudt_usdt_collision(&[candidate_table.as_slice()])
            {
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    format!(
                        "PDU_ERR_FCT_FAILED: UniqueRespIdTable rejected -- CAN address {collision:?} \
                         is configured as both a flow-control-eligible CP_CanRespUSDTId and a \
                         CP_CanRespUUDTId; under native-mixed CAN mode SAE J2534-2 clause 8's \
                         CAN_MIXED_FORMAT_ON semantics always route a match on that address to the \
                         FLOW_CONTROL_FILTER, so the UUDT interpretation can never be observed"
                    ),
                    PduError::PduErrFctFailed,
                    last_error,
                ));
            }
        }

        let (device_guard, device_id) = self.ensure_open_device(handle).await?;

        // ADR-134 (closing the residual ADR-131's Amendment left open):
        // reject here, still holding `device_guard`, if a hard channel error
        // has marked this module PduModstNotAvail -- otherwise a CLL created
        // while Ready could still connect (opening a physical channel or
        // joining an existing one) after the module went NotAvail, bypassing
        // ModuleConnect's now-sticky rejection at the CLL-communication
        // level. `device_guard` stays held through the entire join-or-create
        // decision and native PassThruConnect call below, so no concurrent
        // ModuleDisconnect (which needs this same device_id lock) can
        // interleave between this check and the physical connect completing
        // -- the same discipline as rpc_module_connect/
        // rpc_create_com_logical_link. This RPC has no module_handle of its
        // own (only cll_handle, and this service opens at most one device at
        // a time -- ADR-107), so `requested` is read from the guard's own
        // (u32, DeviceId) content rather than from the request. ISO 22900-2
        // Table 19 (§9.4.11.5, PDUConnect) lists PDU_ERR_MODULE_NOT_CONNECTED
        // as a legal return.
        // `module_state` is checked and released before `shared_channels` is
        // acquired below, preserving the established device_id ->
        // module_state -> shared_channels lock order. The `dead`-channel
        // rejection further down needs its own fresh `last_error` read (it
        // must not re-acquire `module_state` while holding
        // `shared_channels`), so no snapshot is carried out of this block.
        {
            let module_state = self.module_state.lock().await;
            if module_state.status != vci_service_interface::PduModuleStatus::PduModstReady {
                let (open_handle, _) =
                    (*device_guard).expect("ensure_open_device just confirmed a device is open");
                let last_error = module_state.last_error.clone();
                return Err(Self::module_not_avail_status(open_handle, last_error));
            }
        }

        let channel_key: ChannelKey = (
            j2534_proto_id,
            baud_rate,
            pin_select,
            fd_data_phase_rate.unwrap_or(0),
        );

        // Hold shared_channels lock for the entire check-create-insert sequence to prevent
        // a TOCTOU race: two concurrent ConnectComLogicalLink calls with the same channel_key
        // must not both call PassThruConnect and create duplicate physical channels.
        let mut chans = self.shared_channels.lock().await;

        let (channel_id, is_new_channel) = if let Some(sc) = chans.get(&channel_key) {
            // ADR-134 Correction (Codex review round 6): reject joining a
            // channel `events::handle_channel_hard_error` has marked `dead`
            // -- its poll task is exiting (or has already exited), so
            // reusing `sc.channel_id` here would publish this CLL as
            // `connected` on a channel with nothing left to service its
            // RX/TX, a permanent silent hang the module going `NotAvail`
            // cannot self-heal (unlike a brand-new channel, which fails its
            // own first RX poll if the underlying device is genuinely
            // dead). Checked first, before the TOCTOU discipline this
            // critical section already provides for the filter checks
            // below -- `dead` is set under this same `shared_channels`
            // guard, so no interleaving hard error can flip it after this
            // read.
            if sc.dead {
                let (open_handle, _) =
                    (*device_guard).expect("ensure_open_device just confirmed a device is open");
                // Read `last_error` fresh from `module_state`, immediately
                // before this rejection: `events::handle_channel_hard_error`
                // sets `sc.dead` under `shared_channels` before it updates
                // `module_state.last_error`/`status`, several `.await`s
                // later, so a snapshot taken before this critical section
                // could be stale for a connect landing in that window.
                // `shared_channels` is dropped first to preserve the
                // established device_id -> module_state -> shared_channels
                // lock order (never held simultaneously).
                drop(chans);
                let last_error = self.module_state.lock().await.last_error.clone();
                return Err(Self::module_not_avail_status(open_handle, last_error));
            }
            // SAE J2534-2 clause 11 GM UART Phase 8 (ADR-189, Codex review P2
            // fix, PR #98): reject joining a channel with a
            // `PDU_IOCTL_BECOME_MASTER` bid currently in flight
            // (`SharedChannel::become_master_in_flight` -- see its own doc
            // comment for the full race this closes). Checked in the same
            // TOCTOU-preventing critical section as every other join
            // rejection here (`dead` just above, the filter-conflict checks
            // below) -- a bid armed concurrently under this same
            // `shared_channels` lock cannot slip past this read, and this is
            // exactly the window `ioctl_become_master`'s own fix needed
            // closed on the join side.
            if sc.become_master_in_flight.load(Ordering::SeqCst) {
                let last_error = {
                    let links = self.logical_links.lock().await;
                    links.get(&handle).and_then(|l| l.last_error.clone())
                };
                return Err(gm_uart_shared_channel_locked_status(
                    "PDU_IOCTL_BECOME_MASTER",
                    "a PDU_IOCTL_BECOME_MASTER bid is already in flight on this physical \
                     channel -- BECOME_MASTER is bounded to ~2s per SAE J2534-2 clause \
                     11.3.3.2 and retryable once it completes",
                    last_error,
                ));
            }
            // A client filter on this channel_id (PDU_IOCTL_START_MSG_FILTER)
            // was only ever allowed to install while its CLL was the sole
            // owner (ioctl_start_msg_filter's shared-channel rejection,
            // Codex-review fix) -- a BLOCK_FILTER in particular would drop
            // this joining CLL's traffic channel-wide. Reject the join rather
            // than silently let a second CLL land on a filtered channel;
            // held within the same TOCTOU-preventing critical section as the
            // ref_count bump below, so no filter can be installed between
            // this check and the join completing.
            //
            // A2-7/ADR-129: symmetric case -- THIS CLL itself may already
            // hold pre-connect filters in `pending_client_filters` (from
            // `PDU_IOCTL_START_MSG_FILTER` before this `PDUConnect`). Joining
            // an already-existing channel with those still un-installed
            // would need to either install them and violate ADR-082's
            // sole-ownership invariant, or silently drop them -- both
            // unacceptable, so the join itself is rejected instead, exactly
            // like the reciprocal direction above. The filters stay pending
            // (not discarded) so the client can `CLEAR_MSG_FILTER` and retry.
            let (filtered_by_another_cll, this_cll_has_pending, last_error) = {
                let links = self.logical_links.lock().await;
                let filtered = links
                    .values()
                    .any(|l| l.channel_id == Some(sc.channel_id) && !l.client_filters.is_empty());
                let this_cll_has_pending = links
                    .get(&handle)
                    .is_some_and(|l| !l.pending_client_filters.is_empty());
                let last_error = links.get(&handle).and_then(|l| l.last_error.clone());
                (filtered, this_cll_has_pending, last_error)
            };
            if filtered_by_another_cll || this_cll_has_pending {
                // No PDUError variant means "function not supported" specifically
                // (that name never existed in the ISO 22900-2 PDUError enum this
                // service exposes); PDU_ERR_FCT_FAILED is the closest real code
                // for an adapter-level structural limitation like this one.
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "PDU_ERR_FCT_FAILED: cannot join a physical channel that \
                     already has a PDU_IOCTL_START_MSG_FILTER filter installed by another \
                     ComLogicalLink, or while this ComLogicalLink has filters pre-configured \
                     via PDU_IOCTL_START_MSG_FILTER before PDUConnect",
                    PduError::PduErrFctFailed,
                    last_error,
                ));
            }
            // SAE J2534-2 clause 10 Analog Inputs (ADR-178, revising
            // ADR-177/Phase 15's edge-case-hunter finding): unlike
            // `fd_data_phase_rate`, `analog_sample_rate` is NOT part of
            // `ChannelKey` (this protocol's `j2534_proto_id` alone already
            // uniquely identifies one of the 32 `ANALOG_IN_x` subsystems,
            // the same reason `pin_select` is never populated for it
            // either) -- so without this check, a second CLL requesting a
            // different rate for the SAME already-open `ANALOG_IN_x`
            // channel would silently join it with its own rate request
            // never applied, no error, no signal. Clause 10 gives no reason
            // two CLLs should ever share one acquisition subsystem at
            // different rates (unlike CAN's legitimate physical-bus
            // sharing), so the join is rejected instead, mirroring the
            // filter-conflict rejection just above.
            //
            // Compares against this channel's recorded APPLIED rate
            // (`sc.applied_analog_sample_rate`), not any sibling CLL's live
            // per-CLL/Working state -- a staged ComParam is re-stageable
            // post-connect, so comparing live Working values again would
            // let an owner's post-connect re-stage (with no reconnect)
            // silently desync this check from what is actually running on
            // the hardware. See `SharedChannel::applied_analog_sample_rate`'s
            // own doc comment.
            if let Some(rate) = analog_sample_rate
                && sc.applied_analog_sample_rate != Some(rate)
            {
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "PDU_ERR_FCT_FAILED: cannot join a SAE J2534-2 clause 10 Analog Input \
                     physical channel that is already active at a different \
                     analog_sample_rate -- another ComLogicalLink already holds this \
                     ANALOG_IN_x resource open at a different rate",
                    PduError::PduErrFctFailed,
                    last_error,
                ));
            }
            // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): Shared-
            // channel join guard. `ChannelKey` cannot distinguish two CLLs
            // staging different `CP_NdisPinOption` values on this protocol,
            // since `baud` and `pin_select` are always `0` for it -- the same
            // hazard class ADR-178 already closed for `CP_AnalogSampleRate`
            // just above. The comparison is in resolved-flag space (this
            // CLL's own `connect_flags`, already resolved via
            // `ndis_pin_option_connect_flags` in the snapshot block above),
            // masked to just the two NDIS pin-option bits, against the
            // physical channel's already-recorded `SharedChannel::connect_flags`
            // -- stamped verbatim at creation from the creating CLL's
            // resolved connect flags and never mutated afterward, so it is
            // reused directly rather than duplicated into a second field.
            // Comparing raw staged-ComParam values instead would spuriously
            // reject two CLLs that in fact resolve to the same applied
            // hardware configuration (e.g. auto vs. an out-of-range value,
            // both mapping to "neither bit").
            if base_proto_id == j2534_0404::PROTOCOL_ETHERNET_NDIS {
                const NDIS_PIN_OPTION_MASK: u32 = j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION1
                    | j2534_0404::CONNECT_FLAG_NDIS_PINS_OPTION2;
                if (connect_flags & NDIS_PIN_OPTION_MASK)
                    != (sc.connect_flags & NDIS_PIN_OPTION_MASK)
                {
                    return Err(state_guard_status(
                        Code::FailedPrecondition,
                        "PDU_ERR_FCT_FAILED: cannot join a SAE J2534-2 clause 24 Ethernet_NDIS \
                         physical channel that is already active with a different resolved \
                         CP_NdisPinOption -- another ComLogicalLink already holds this NDIS \
                         adapter open with a different pin-option connect flag",
                        PduError::PduErrFctFailed,
                        last_error,
                    ));
                }
            }
            // ADR-198 Phase 2: shared-channel join-compatibility check for
            // K-line's manual-checksum connect flag -- mirrors the
            // Ethernet_NDIS pin-option join guard immediately above exactly.
            // `ChannelKey` cannot distinguish two K-line CLLs with
            // disagreeing effective `ISO9141_NO_CHECKSUM` bits (the key is
            // `(protocol, baud, pin_select)`, none of which encode RawMode/
            // ChecksumMode), so a RawMode=OFF CLL (whose effective bit is
            // always 0, since ChecksumMode is ignored when RawMode is OFF --
            // Table D.6) must never be allowed to join a RawMode=ON/
            // ChecksumMode=OFF (NO_CHECKSUM=1) channel, and vice versa: the
            // two CLLs would disagree about whether frames on the wire carry
            // a manually-managed checksum byte. Compares this CLL's own
            // already-resolved `connect_flags` against the physical
            // channel's recorded `SharedChannel::connect_flags` (stamped
            // verbatim at creation from the creating CLL's resolved connect
            // flags, ADR-044 creator-decides), masked to just the one bit --
            // never raw ComParam/RawMode/ChecksumMode values directly, which
            // would risk drifting from what actually got resolved.
            if base_proto_id == j2534_0404::ISO9141 || base_proto_id == j2534_0404::ISO14230 {
                let mask = j2534_0404::ISO9141_NO_CHECKSUM;
                if (connect_flags & mask) != (sc.connect_flags & mask) {
                    return Err(state_guard_status(
                        Code::FailedPrecondition,
                        "PDU_ERR_FCT_FAILED: cannot join a K-line (ISO9141/ISO14230) physical \
                         channel that is already active with a different resolved \
                         ISO9141_NO_CHECKSUM connect flag -- another ComLogicalLink already \
                         holds this channel open with a different effective RawMode/ChecksumMode \
                         combination (ADR-198)",
                        PduError::PduErrFctFailed,
                        last_error,
                    ));
                }
            }
            (sc.channel_id, false)
        } else {
            // ADR-156 Decision 4/Phase 2b design review correction: a
            // `_CHx` link opening a brand-new physical channel gets a
            // synchronous capacity precheck against the cached
            // `DEVICE_INFO_<PROTOCOL>_SUPPORTED` Discovery answer (ADR-153)
            // before the native connect -- "Discovery refines, native error
            // is the fallback" (ADR-153 Decision 1): nothing cached (or not
            // `Supported`) falls through to the native `PassThruConnect`
            // error unchanged. Only for a brand-new channel: a joining CLL
            // reuses an already-successfully-opened physical channel, which
            // already proved capacity was available.
            //
            // Keyed on `j2534_proto_id`, not `base_proto_id` (Codex review
            // correction, PR #124, ADR-211): `check_chx_capacity` internally
            // calls `resources::chx_device_info_supported_parameter`, which
            // now performs its own FT-aware normalization -- passing the
            // already-fully-normalized `base_proto_id` here would collapse a
            // Fault-Tolerant CAN `_PS`/`_CHx` link down to plain `CAN`/
            // `ISO15765`, consulting the wrong `DEVICE_INFO_*_SUPPORTED`
            // flag. Same rationale as `connect_discovery_check`'s own
            // `j2534_proto_id`-keyed call a few lines below.
            // ADR-185 second lock-order fix (design-advisor consult): both
            // `check_chx_capacity` and `enforce_discovery_capability` below
            // are reachable while `chans` (`self.shared_channels`) is held
            // in this function's check-create-insert region, and their
            // native-call error branches used to read `self.module_state`
            // internally -- inverting the documented `device_id ->
            // module_state -> shared_channels` order (ADR-107 addendum/
            // ADR-134). Both now take a caller-supplied `last_error`
            // snapshot instead; this read (previously taken only for the
            // `enforce_discovery_capability` call below) is hoisted above
            // `check_chx_capacity` so both calls share the same snapshot.
            // Distinct from the OTHER match arm's own `last_error` variable
            // (the join-existing-channel path above) -- different `if`/
            // `else` branch, no collision.
            let last_error = self
                .logical_links
                .lock()
                .await
                .get(&handle)
                .and_then(|l| l.last_error.clone());
            if let Some(channel_index) = link_channel_index {
                let (module_handle, _) =
                    (*device_guard).expect("ensure_open_device just confirmed a device is open");
                // `device_id` (not re-derived from `device_guard`) is passed
                // explicitly: `check_chx_capacity` must not call
                // `ensure_open_device_for` itself, since `device_guard` --
                // acquired above and still held here -- already holds the
                // same `self.device_id` mutex a second acquisition would
                // deadlock against (see `check_chx_capacity`'s own doc
                // comment).
                self.check_chx_capacity(
                    module_handle,
                    device_id,
                    j2534_proto_id,
                    channel_index,
                    last_error.clone(),
                )
                .await?;
            }
            // ADR-185 Stage 1 (Discovery-cache connect-time enforcement):
            // independent of the `_CHx` capacity precheck just above -- a
            // different resource-capability question (does the device
            // support THIS protocol family at all, vs. how many `_CHx`
            // indices it has for a family it already supports) -- so both
            // run, not one instead of the other. Runs for every brand-new-
            // channel connect where `resources::connect_discovery_check`
            // has a `j2534_proto_id` mapping (SWCAN, FT-CAN, UART Echo Byte,
            // Honda DIAG-H, J1708, Analog Inputs, TP2.0, GM UART, and
            // Ethernet_NDIS); a no-op (`Ok(())`, no
            // Discovery query at all) for every other protocol family,
            // exactly like the `_CHx` precheck's own no-op shape. Keyed on
            // `j2534_proto_id`, not `base_proto_id` -- see
            // `resources::connect_discovery_check`'s own doc comment for why
            // a base-keyed lookup would check the wrong `DEVICE_INFO_*` flag
            // entirely.
            if let Some(check) = resources::connect_discovery_check(j2534_proto_id) {
                let (module_handle, _) =
                    (*device_guard).expect("ensure_open_device just confirmed a device is open");
                self.enforce_discovery_capability(
                    module_handle,
                    discovery::DeviceAccess::AlreadyOpen(device_id),
                    check,
                    "ConnectComLogicalLink",
                    PduError::PduErrIdNotSupported,
                    last_error,
                )
                .await?;
            }
            let channel_id = self
                .connect_new_physical_channel(
                    handle,
                    NewPhysicalChannelParams {
                        device_id,
                        j2534_proto_id,
                        baud_rate,
                        connect_flags,
                        working_snapshot: &working_snapshot,
                        pin_select: link_pin_select,
                        fd_data_phase_rate,
                        // ADR-160 Correction (Codex review on the
                        // SET_CONFIG-ordering fix PR): the qualified-link
                        // exclusion is already folded into
                        // `native_mixed_applies` above (ADR-217) -- a
                        // pin-selected/`_CHx`-qualified link never gets the
                        // native-mixed `PASS_FILTER` treatment, so it must
                        // not have `CONFIG_CAN_MIXED_FORMAT` turned on for
                        // its physical channel either. Leaving it on for a
                        // qualified link would (per SAE J2534-2 clause 8
                        // Figure 1) route every `FLOW_CONTROL_FILTER` match
                        // into ISO15765 reassembly before `PASS_FILTER` ever
                        // runs, silently misparsing/losing UUDT responses on
                        // that channel for no corresponding benefit --
                        // nothing downstream ever exploited the bit being
                        // set for a qualified link.
                        native_mixed_format: native_mixed_set_config_value,
                        analog_sample_rate,
                    },
                )
                .await?;
            (channel_id, true)
        };

        // A2-7/ADR-129: install any filters pre-configured via
        // PDU_IOCTL_START_MSG_FILTER before this connect (ISO 22900-2
        // §9.4.11.2 d). Only reachable with `is_new_channel` -- joining an
        // existing channel while holding pending filters was just rejected
        // above -- so this CLL is guaranteed sole owner of `channel_id`,
        // matching ADR-082's invariant. Runs before `spawn_new_shared_channel`
        // so a failure's rollback is just disconnecting the channel this
        // call itself created: no `SharedChannel` entry or poll task exists
        // yet for anything else to unwind.
        let installed_pending_filters = if is_new_channel {
            let pending: Vec<vci_service_interface::IoFilter> = {
                let links = self.logical_links.lock().await;
                links
                    .get(&handle)
                    .map(|l| l.pending_client_filters.values().cloned().collect())
                    .unwrap_or_default()
            };
            if pending.is_empty() {
                HashMap::new()
            } else {
                let api = self.api.lock().await;
                let install_result = install_client_message_filters(
                    &api,
                    handle,
                    channel_id,
                    j2534_proto_id,
                    connect_flags,
                    &pending,
                )
                .await;
                drop(api);
                match install_result {
                    Ok(map) => map,
                    Err(failure) => {
                        let api = self.api.lock().await;
                        let _ = api.disconnect(channel_id);
                        drop(api);
                        return Err(match failure {
                            InstallFilterFailure::Status(status) => status,
                            InstallFilterFailure::Native(err) => {
                                let last_error = self
                                    .logical_links
                                    .lock()
                                    .await
                                    .get(&handle)
                                    .and_then(|l| l.last_error.clone());
                                map_native_error_for_link(
                                    "PassThruStartMsgFilter",
                                    &err,
                                    last_error,
                                )
                            }
                        });
                    }
                }
            }
        } else {
            HashMap::new()
        };

        // ADR-161 rollback sub-rule: `Some((stamped_epoch, prev_epoch))` only
        // for the join-an-existing-channel branch -- a rolled-back
        // `is_new_channel` join removes the whole entry
        // (`finalize_connected_link`'s rollback path) and so has nothing to
        // restore.
        let join_epoch_rollback: Option<(u64, u64)> = if is_new_channel {
            self.spawn_new_shared_channel(
                channel_key,
                channel_id,
                connect_flags,
                analog_sample_rate,
                analog_samples_per_reading,
                analog_readings_per_msg,
                // ADR-194/Phase 16: `false` only for an Ethernet_NDIS
                // channel -- `j2534_proto_id` is this connect's raw
                // (post-substitution) hardware protocol id, matching every
                // other Plane A use in this function.
                j2534_proto_id != j2534_0404::PROTOCOL_ETHERNET_NDIS,
                &mut chans,
            )
            .await;
            None
        } else {
            // ADR-161: re-stamp the occupancy epoch on every `ref_count`
            // increment, not just at creation, so `ioctl_reset`'s Phase 1 can
            // detect this join even though it never touches `connect_generation`.
            let stamped_occupancy_epoch = self.next_occupancy_epoch().await;
            let sc = chans.get_mut(&channel_key).expect(
                "channel_key was just found in chans above, and chans has not been unlocked since",
            );
            let prev_occupancy_epoch = sc.occupancy_epoch;
            sc.ref_count += 1;
            sc.occupancy_epoch = stamped_occupancy_epoch;
            Some((stamped_occupancy_epoch, prev_occupancy_epoch))
        };
        // `device_id` is unused past this point in this function, so this is
        // the last moment it's safe to drop the guard -- and the earliest.
        // It MUST be dropped here rather than left to end-of-scope: later in
        // this function, `probe_can_channel_mode` and
        // `ensure_uudt_companion_channel` each acquire `device_id`
        // themselves, and `tokio::sync::Mutex` is non-reentrant, so still
        // holding this guard when either is reached would self-deadlock
        // (ADR-107 addendum (i), Codex review on PR #110).
        drop(device_guard);

        // SAE J2534-2 clause 10 Analog Inputs (ADR-216 Decision items 6 and
        // 10, amended by Codex review, PR #130, Finding 1): the capability
        // `GET_CONFIG` read and the two batching values are captured HERE --
        // right after `device_guard` is dropped, needing no additional
        // locking beyond what `read_analog_capability_params` itself takes
        // internally (`self.api.lock()`), and BEFORE `finalize_connected_
        // link`'s own critical section begins -- so both can be threaded
        // into that critical section as a single parameter instead of being
        // applied by a separate call running AFTER `connected`/`channel_key`
        // are already published (see `finalize_connected_link`'s own doc
        // comment for the race this closes). Every connect, fresh and join
        // alike, adopts the channel's live/applied acquisition configuration
        // -- unlike every other ComParam, a joining CLL gets no Working→
        // Active push at all otherwise (`finalize_connected_link`'s own
        // `is_new_channel` gate), but this subsystem's five capability/
        // RO-ish ComParams and its two batching ComParams are each sourced
        // from the physical channel, not from any CLL's own staged Working
        // set, so the join/new-channel distinction this service otherwise
        // draws does not apply to either group -- a joiner never keeps its
        // own seeded default. The two groups differ only in HOW they are
        // sourced: `read_analog_capability_params` does a live `GET_CONFIG`
        // (clause 10.3.3.2.1's own device-dependent default for
        // `CP_AnalogActiveChannels` means even a fresh connect needs a live
        // read, not just the four genuinely read-only-ish params) and is
        // best-effort (see its own doc comment for failure handling); the
        // batching values are sourced from `analog_samples_per_reading`/
        // `analog_readings_per_msg` (the same in-scope Working-snapshot
        // locals already computed above in this function) for a fresh
        // connect, or from `chans.get(&channel_key)`'s own `SharedChannel::
        // applied_analog_samples_per_reading`/`applied_analog_readings_
        // per_msg` for a join -- reading `chans` here is safe because it is
        // the SAME `shared_channels` guard already held continuously since
        // before this point (never re-locked), and stays held straight
        // through into `finalize_connected_link` below.
        let analog_connect_sync: AnalogConnectSync =
            if resources::is_analog_in_protocol_id(base_proto_id) {
                let capability = self.read_analog_capability_params(channel_id).await;
                let (samples_per_reading, readings_per_msg) = if is_new_channel {
                    (analog_samples_per_reading, analog_readings_per_msg)
                } else {
                    chans.get(&channel_key).map_or((None, None), |sc| {
                        (
                            sc.applied_analog_samples_per_reading,
                            sc.applied_analog_readings_per_msg,
                        )
                    })
                };
                (capability, samples_per_reading, readings_per_msg)
            } else {
                (None, None, None)
            };

        // ADR-123 Fix E: the `tx_suspended_by_lock` recompute sweep (this CLL
        // may need to inherit a sibling's held `LOCK_PHYSICAL_TX_QUEUE`, spec
        // line 1810) now runs INSIDE `finalize_connected_link`, in the same
        // critical section that publishes `connected`/`channel_key` -- not
        // here as a separate, later re-acquisition of `shared_channels` then
        // `logical_links`. See `finalize_connected_link`'s doc comment.
        //
        // A2-7/ADR-129: `chans` (this function's own `shared_channels` guard)
        // is passed through and stays locked into `finalize_connected_link`
        // rather than being dropped first -- closing a race where a
        // concurrent CLL's join-guard check above could otherwise run in the
        // gap between this channel's creation/filter-install and
        // `channel_id`/`client_filters` becoming visible. See
        // `finalize_connected_link`'s doc comment.
        self.finalize_connected_link(
            handle,
            channel_id,
            channel_key,
            is_new_channel,
            working_snapshot,
            installed_pending_filters,
            &mut chans,
            join_epoch_rollback,
            // ADR-162 Decision 2: same gate as the earlier connect-time
            // self-check above -- native-mixed mode, unqualified link.
            // ADR-217: `native_mixed_collision_enforced` already folds in
            // both conditions and stays `NativeMixed`-only.
            native_mixed_collision_enforced,
            analog_connect_sync,
        )
        .await?;
        drop(chans);

        // Build ISO15765 point-to-point FLOW_CONTROL_FILTERs directly from this
        // CLL's UniqueRespIdTable as configured up to this point — no wide-open
        // pass-all fallback is installed at connect (ADR-048). A table already
        // set via SetUniqueRespIdTable before Connect takes effect immediately;
        // an empty table (addressing configured after Connect, the common case)
        // installs nothing, so the CLL receives only what other CLLs sharing the
        // channel already cause the adapter to forward, until its own
        // SetUniqueRespIdTable call installs filters of its own.
        if base_proto_id == j2534_0404::ISO15765 {
            let entries = {
                let links = self.logical_links.lock().await;
                links
                    .get(&handle)
                    .map(|l| l.active_unique_resp_id_table.clone())
                    .unwrap_or_default()
            };
            let filter_ids = self
                .install_point_to_point_fc_filters(
                    channel_id,
                    &entries,
                    j2534_proto_id,
                    link_pin_select.is_some()
                        || link_channel_index.is_some()
                        || resources::is_fd_protocol_id(j2534_proto_id),
                )
                .await;
            if let Some(link) = self.logical_links.lock().await.get_mut(&handle) {
                link.unique_resp_filter_ids = filter_ids;
            }
        }

        // Auto mode: the first ISO15765-family CLL to establish a *new*
        // physical ISO15765 channel decides, once, whether this device is
        // dual-channel-capable (ADR-046 addendum). A no-op for every other
        // mode and for joining CLLs (whose channel's creator already probed).
        // ADR-157 accepted residual, widened to `_CHx` by ADR-156 Decision 3
        // addendum/Phase 2b: also skipped for a qualified (`_PS` or `_CHx`)
        // link -- an `ISO15765_PS`/`ISO15765_CHx` link always uses hardware
        // ISO-TP, so it must never be the one that decides (or is affected
        // by) the module-wide dual-channel-mode probe.
        if is_new_channel
            && base_proto_id == j2534_0404::ISO15765
            && link_pin_select.is_none()
            && link_channel_index.is_none()
            && !resources::is_fd_protocol_id(j2534_proto_id)
        {
            self.probe_can_channel_mode(handle).await;
        }

        // Dual-channel mode: when the UniqueRespIdTable already configures a
        // UUDT response ID (set before connect), open the companion raw-CAN
        // channel now; otherwise it is opened by SetUniqueRespIdTable when a
        // UUDT ID first appears (ADR-046).
        //
        // ADR-157 accepted residual (found by Codex review, PR #28), widened
        // to `_CHx` by ADR-156 Decision 3 addendum/Phase 2b: also skipped
        // for a qualified (`_PS` or `_CHx`) link, mirroring the
        // `probe_can_channel_mode` skip just above for the same reason.
        // `ensure_uudt_companion_channel` always opens a plain (unqualified)
        // raw-CAN companion channel on the link's default pins, discarding
        // `pin_select`/`channel_index` entirely -- for a qualified
        // `ISO15765_PS`/`CAN_PS`/`ISO15765_CHx`/`CAN_CHx` link this would
        // route UUDT traffic to a companion channel on the wrong physical
        // resource. Building a qualifier-aware companion channel (its own
        // `_PS`/`_CHx` variant, its own `SET_CONFIG`/vendor-connector open,
        // its own `ChannelKey` entry) is a substantial new mechanism
        // deliberately out of scope here, same as the software-ISO-TP x Pin
        // Selection residual above -- dual-channel-mode UUDT capture is
        // simply skipped, not attempted, for a qualified link.
        if self.effective_can_channel_mode().await == CanChannelMode::DualChannel
            && CanChannelMode::applies_to(protocol)
            && has_uudt_ids
            && link_pin_select.is_none()
            && link_channel_index.is_none()
            && !resources::is_fd_protocol_id(j2534_proto_id)
            && let Err(status) = self.ensure_uudt_companion_channel(handle).await
        {
            warn!(cll_handle = handle, %status, "failed to open UUDT companion CAN channel");
        }

        events::send_cll_status(
            &self.subscriptions,
            &self.logical_links,
            handle,
            vci_service_interface::PduComLogicalLinkStatus::PduCllstOnline,
        )
        .await;

        info!(
            cll_handle = handle,
            protocol = protocol.value(),
            j2534_proto_id,
            baud_rate,
            is_new_channel,
            "ConnectComLogicalLink"
        );
        Ok(Self::empty_response())
    }

    /// Installs point-to-point filters on `channel_id` for every entry in
    /// `entries` that carries a `CP_CanRespUSDTId` and/or `CP_CanRespUUDTId`:
    /// up to two filters per entry, one per response address that is
    /// present, both sharing the same `CP_CanPhysReqId` flow-control address
    /// when the underlying filter is a `FLOW_CONTROL_FILTER` (ISO 22900-2
    /// documents `CP_CanPhysReqId` as used for flow-control CAN transmission
    /// for both the USDT and UUDT cases). An entry missing `CP_CanPhysReqId`
    /// skips only its `FLOW_CONTROL_FILTER`-based filter(s) — a
    /// `pFlowControlMsg` is required to build a spec-conformant
    /// point-to-point `FLOW_CONTROL_FILTER` regardless of response type
    /// (ADR-039, ADR-041) — but NOT its native-mixed `PASS_FILTER` (see
    /// "Native-mixed mode" below), which has no `pFlowControlMsg` and never
    /// needs a request address (Finding 2, Codex review PR #32:
    /// `SetUniqueRespIdTable` does not require `CP_CanPhysReqId` to
    /// accompany a `CP_CanRespUUDTId`, so a receive-only UUDT entry is valid
    /// input).
    ///
    /// The USDT filter is additionally skipped when `CP_CanRespUSDTFormat`
    /// explicitly disables flow control (Table B.13 bit 0) — installing a
    /// `FLOW_CONTROL_FILTER` for an address that never uses ISO 15765-2 flow
    /// control is pointless (ADR-040). The UUDT filter has no such gate: flow
    /// control is inherently not applicable to unsegmented UUDT addressing, so it
    /// is installed whenever `CP_CanRespUUDTId` is present AND not the spec's
    /// `0xFFFFFFFF` "not used" sentinel (A2-12 Codex follow-up; see
    /// [`uudt_resp_id`]), regardless of `CP_CanRespUUDTFormat` bit 0 (ADR-041).
    ///
    /// `CP_Can{RespUSDT,RespUUDT,PhysReq}Format` / `*ExtAddr`, when present, select
    /// 4- vs. 5-byte (extended-addressing) filter messages and the 11-/29-bit CAN
    /// Id `TxFlags` for their respective address; when absent, normal 11-bit
    /// addressing with flow control enabled is assumed (ADR-039's original,
    /// format-unaware behavior). Individual `PassThruStartMsgFilter` failures are
    /// logged and skipped rather than aborting the whole table, mirroring
    /// ADR-008's best-effort posture.
    ///
    /// `protocol_id` is `channel_id`'s actual connect-time `hw_protocol_id`
    /// (ADR-157 Plane A, raw -- e.g. `ISO15765_PS` for a pin-selected link,
    /// not a normalized/base id), threaded into every mask/pattern/
    /// flow-control message built here so its `ProtocolID` field matches the
    /// literal id the channel was opened with (found by Codex review, PR #28;
    /// a mismatch can be rejected by a conforming adapter with
    /// `ERR_MSG_PROTOCOL_ID`, silently leaving the link filterless).
    ///
    /// `qualified` is `true` when the connecting link carries EITHER SAE
    /// J2534-2 connect-time resource qualifier (ADR-156 Decision 2's
    /// `pin_select` OR Decision 3/Phase 2b's `channel_index`, i.e.
    /// `link.pin_select.is_some() || link.channel_index.is_some()`) --
    /// `false` for an unqualified link. A companion raw-CAN channel
    /// (`ensure_uudt_companion_channel`) is only ever opened for an
    /// unqualified link (ADR-157's `link_pin_select.is_none()` guards in
    /// `rpc_connect_com_logical_link`/`promote_unique_resp_id_table`,
    /// widened to `qualified` by ADR-156 Decision 3 addendum/Phase 2b), so
    /// the module-wide `dual_channel` mode flag alone is not sufficient to
    /// decide whether a companion actually exists for THIS link -- a
    /// qualified link never gets one regardless of the module's
    /// dual-channel setting, and must therefore still receive the
    /// point-to-point UUDT fallback filter this function installs (fix for
    /// a bug found by `edge-case-hunter`, PR #28, originally for `_PS`
    /// only: without this, a qualified link in dual-channel mode got
    /// neither a companion channel nor this fallback filter -- total,
    /// silent loss of UUDT response capture). This condition and the
    /// companion-channel-skip condition at each of this function's three
    /// call sites must move together -- letting them drift apart reproduces
    /// exactly that bug (edge-case-hunter finding, Phase 2a; ADR-156
    /// Decision 3 addendum flags it as a known hazard for `_CHx` too).
    ///
    /// **A third disqualifier, alongside `pin_select`/`channel_index`
    /// (ADR-159/Phase 3b):** an FD-substituted link (`resources::is_fd_
    /// protocol_id(hw_protocol_id)`, e.g. `FD_ISO15765_PS`) is FD
    /// substitution's own connect-time inference, independent of Pin
    /// Selection/Additional Channels -- but it disqualifies a link from the
    /// Classic-CAN UUDT companion channel exactly like the other two do (a
    /// Classic-format companion channel would be the wrong physical resource
    /// for an FD-connected link), so it must be `||`-ed into `qualified` at
    /// every one of this function's three call sites too, in the same
    /// lockstep as `pin_select`/`channel_index` above.
    ///
    /// **Native-mixed mode (ADR-160/Phase 3c; widened to the family by
    /// ADR-217):** an unqualified link on a channel in either native-mixed
    /// sub-mode (`CanChannelMode::is_native_mixed_family()` --
    /// `NativeMixed` or `NativeMixedAllFrames`) gets a genuine `PASS_FILTER`
    /// (narrowed to the UUDT id, no `pFlowControlMsg`) instead of the
    /// ADR-041 `FLOW_CONTROL_FILTER` workaround for its UUDT response id --
    /// the channel's own connect-time `SET_CONFIG(CAN_MIXED_FORMAT, value)`
    /// (`connect_new_physical_channel`) plus this filter are enough for the
    /// device to deliver UUDT frames with a raw-CAN native `ProtocolID`
    /// (`events.rs`'s per-frame routing branch tells them apart from
    /// ISO15765 traffic) -- the installed filter is identical either way,
    /// only the earlier connect-time `SET_CONFIG` value differs by
    /// sub-mode. Mutually exclusive with `dual_channel` by construction (see
    /// `native_mixed`'s own doc comment below).
    pub(super) async fn install_point_to_point_fc_filters(
        &self,
        channel_id: ChannelId,
        entries: &[EcuUniqueRespEntry],
        protocol_id: u32,
        qualified: bool,
    ) -> Vec<MessageFilterId> {
        // Computed BEFORE `api` is locked below (Fix for an AB-BA deadlock
        // found on PR #110's holistic audit): `probe_can_channel_mode` holds
        // `resolved_can_channel_mode`'s lock while awaiting `api` (via
        // `ensure_uudt_companion_channel`), so calling
        // `effective_can_channel_mode()` -- which locks
        // `resolved_can_channel_mode` -- while THIS function holds `api`
        // would AB-BA-deadlock against a concurrent `probe_can_channel_mode`.
        // General rule: never call `effective_can_channel_mode()` while
        // holding `api` or `device_id`. Read once and reused for both
        // `dual_channel` and `native_mixed` below (ADR-217 -- mirrors
        // `rpc_connect_com_logical_link`'s own single-read consolidation for
        // its three native-mixed locals), rather than two separate awaited
        // locks of `resolved_can_channel_mode` for a value that cannot
        // change between them within this call.
        let effective_can_mode = self.effective_can_channel_mode().await;
        let dual_channel = effective_can_mode == CanChannelMode::DualChannel && !qualified;
        // SAE J2534-2 clause 8 Mixed Format Frames on a CAN Network
        // (ADR-160/Phase 3c; widened to the family by ADR-217): mirrors
        // `dual_channel`'s exact shape -- computed from the same
        // pre-`api`-lock read, for the same AB-BA-deadlock reason, and
        // excludes a qualified (pin-selected/`_CHx`) link the same way (that
        // link's own FLOW_CONTROL_FILTER fallback below is unaffected by
        // native-mixed mode). Uses `is_native_mixed_family()` rather than a
        // literal `== NativeMixed` -- the installed filter type doesn't
        // depend on which native-mixed sub-mode is active, only on whether
        // one is (ADR-217 Decision item 3); unlike the ADR-162 collision
        // checks, this call site has no `ON`-only semantics to preserve.
        // Mutually exclusive with `dual_channel` by construction -- a
        // channel's `CanChannelMode` resolves to exactly one variant -- so
        // at most one of "companion channel," "FC-filter workaround," or
        // "native PASS_FILTER" ever applies to a given UUDT id.
        let native_mixed = effective_can_mode.is_native_mixed_family() && !qualified;
        let api = self.api.lock().await;
        let mut ids = Vec::new();
        for entry in entries {
            // Finding 2 (Codex review, PR #32): `req` is only REQUIRED (and
            // its absence only skips) the FLOW_CONTROL_FILTER-based branches
            // below -- a native-mixed `PASS_FILTER` has no `pFlowControlMsg`
            // and never reads `req` at all (`install_point_to_point_fc_
            // filter` only builds/uses it when `filter_type ==
            // FLOW_CONTROL_FILTER`), so a UUDT-only entry with no
            // `CP_CanPhysReqId` (valid per `SetUniqueRespIdTable`) must still
            // reach that branch instead of being skipped here.
            let req: Option<CanAddress> = entry
                .params
                .unum32
                .get(&PARAM_CAN_PHYS_REQ_ID)
                .copied()
                .map(|req_id| {
                    let req_format = entry
                        .params
                        .unum32
                        .get(&PARAM_CAN_PHYS_REQ_FORMAT)
                        .map_or_else(CanIdFormat::default, |&raw| CanIdFormat::from_raw(raw));
                    CanAddress {
                        id: req_id,
                        format: req_format,
                        ext_addr: entry
                            .params
                            .unum32
                            .get(&PARAM_CAN_PHYS_REQ_EXT_ADDR)
                            .copied()
                            .unwrap_or(0) as u8,
                    }
                });

            // Single source of truth for this entry's go/no-go decision
            // (ADR-222 round 6) -- also reused by `build_cll_rx_entries`'s
            // contention-map contribution gate.
            let elig = point_to_point_filter_eligibility(entry, dual_channel, native_mixed);

            // Codex review PR #42, round 19 follow-up: this USDT branch used
            // to read CP_CanRespUSDTId via a raw, unfiltered key lookup,
            // unlike the sibling UUDT branch below (which already correctly
            // used `uudt_resp_id`) -- a client that leaves CP_CanRespUSDTId
            // at ISO 22900-2 Table 76's `0xFFFFFFFF` "not used" sentinel
            // (spec-legal, unenforced by `SetUniqueRespIdTable`) with the
            // flow-control format bit set reached this branch and installed
            // a real `PassThruStartMsgFilter(FLOW_CONTROL_FILTER)` for CAN ID
            // `0xFFFFFFFF`, outside the valid 11/29-bit range -- real device
            // I/O, not just a comparison template. Now routed through
            // `usdt_resp_id`, mirroring the UUDT branch exactly.
            if let Some(resp_id) = usdt_resp_id(entry) {
                let resp_format = entry
                    .params
                    .unum32
                    .get(&PARAM_CAN_RESP_USDT_FORMAT)
                    .map_or_else(CanIdFormat::default, |&raw| CanIdFormat::from_raw(raw));
                if resp_format.flow_control_enabled {
                    if elig.usdt {
                        let req = req
                            .as_ref()
                            .expect("elig.usdt implies req.is_some() -- see point_to_point_filter_eligibility");
                        let resp = CanAddress {
                            id: resp_id,
                            format: resp_format,
                            ext_addr: entry
                                .params
                                .unum32
                                .get(&PARAM_CAN_RESP_USDT_EXT_ADDR)
                                .copied()
                                .unwrap_or(0) as u8,
                        };
                        match install_point_to_point_fc_filter(
                            &api,
                            channel_id,
                            j2534_0404::FLOW_CONTROL_FILTER,
                            &resp,
                            Some(req),
                            protocol_id,
                        ) {
                            Ok(id) => ids.push(id),
                            Err(err) => warn!(
                                channel_id = channel_id.0, resp_id, req_id = req.id, %err,
                                "failed to install point-to-point FLOW_CONTROL_FILTER (USDT)",
                            ),
                        }
                    } else {
                        // Preserves the pre-existing requirement that a
                        // `FLOW_CONTROL_FILTER` needs a request address
                        // (ADR-039) -- only this specific filter is skipped,
                        // not the whole entry.
                        warn!(
                            channel_id = channel_id.0,
                            resp_id,
                            "skipping point-to-point FLOW_CONTROL_FILTER (USDT): entry has no CP_CanPhysReqId",
                        );
                    }
                }
            }

            // In dual-channel mode UUDT frames are received on the companion
            // raw-CAN channel, so the ADR-041 UUDT FLOW_CONTROL_FILTER
            // workaround on the ISO15765 channel is not installed (ADR-046).
            // `dual_channel` above is already narrowed to "a companion
            // channel actually exists for this link" (module dual-channel
            // mode AND not pin-selected, ADR-157) -- a pin-selected link
            // never gets a companion, so it always falls through to install
            // this fallback filter instead.
            if dual_channel {
                continue;
            }

            if let Some(resp_id) = uudt_resp_id(entry) {
                let resp_format = entry
                    .params
                    .unum32
                    .get(&PARAM_CAN_RESP_UUDT_FORMAT)
                    .map_or_else(CanIdFormat::default, |&raw| CanIdFormat::from_raw(raw));
                let resp = CanAddress {
                    id: resp_id,
                    format: resp_format,
                    ext_addr: entry
                        .params
                        .unum32
                        .get(&PARAM_CAN_RESP_UUDT_EXT_ADDR)
                        .copied()
                        .unwrap_or(0) as u8,
                };
                // ADR-160/Phase 3c: under native-mixed mode, a genuine
                // PASS_FILTER narrowed to `resp`'s address replaces the
                // ADR-041 FLOW_CONTROL_FILTER workaround -- the device's own
                // native ProtocolID tagging (Decision 4/`events.rs`) does
                // the USDT-vs-UUDT split per frame, so this filter only
                // needs to admit the UUDT id itself. `install_point_to_point_
                // fc_filter` ignores `req` for a non-FLOW_CONTROL_FILTER
                // `filter_type`, so a `PASS_FILTER` is installed here
                // regardless of whether `req` is `Some` or `None` (Finding 2,
                // Codex review PR #32) -- only the FLOW_CONTROL_FILTER
                // fallback below still requires it.
                let (filter_type, filter_kind, filter_protocol_id) = if native_mixed {
                    (
                        j2534_0404::PASS_FILTER,
                        "PASS_FILTER (UUDT, native-mixed)",
                        resources::mixed_format_can_protocol_id(protocol_id),
                    )
                } else {
                    (
                        j2534_0404::FLOW_CONTROL_FILTER,
                        "FLOW_CONTROL_FILTER (UUDT)",
                        protocol_id,
                    )
                };
                if !elig.uudt {
                    // Previously this exact condition fell through the early
                    // `continue` above with no log at all; an explicit warn
                    // here is a strict improvement, not a behavior change to
                    // preserve (Finding 2, Codex review PR #32). `elig.uudt`
                    // is false here only because `!(native_mixed ||
                    // req.is_some())` -- `uudt_resp_id(entry).is_some()` and
                    // `!dual_channel` are already established by this point
                    // (the enclosing `if let` and the early `continue`
                    // above), so this is exactly the pre-refactor
                    // `filter_type == FLOW_CONTROL_FILTER && req.is_none()`
                    // condition (ADR-222 round 6).
                    warn!(
                        channel_id = channel_id.0,
                        resp_id,
                        "skipping point-to-point FLOW_CONTROL_FILTER (UUDT): entry has no CP_CanPhysReqId",
                    );
                } else {
                    match install_point_to_point_fc_filter(
                        &api,
                        channel_id,
                        filter_type,
                        &resp,
                        req.as_ref(),
                        filter_protocol_id,
                    ) {
                        Ok(id) => ids.push(id),
                        Err(err) => warn!(
                            channel_id = channel_id.0, resp_id, req_id = req.as_ref().map(|r| r.id), %err,
                            "failed to install point-to-point {filter_kind}",
                        ),
                    }
                }
            }
        }
        ids
    }

    /// Removes previously-installed point-to-point filters. Failures are logged
    /// and otherwise ignored: a stale ID (e.g. one already wiped by a prior
    /// `CLEAR_MSG_FILTERS`) is not actionable.
    pub(super) async fn remove_point_to_point_fc_filters(
        &self,
        channel_id: ChannelId,
        ids: &[MessageFilterId],
    ) {
        if ids.is_empty() {
            return;
        }
        let api = self.api.lock().await;
        for &id in ids {
            if let Err(err) = api.stop_message_filter(channel_id, id) {
                warn!(channel_id = channel_id.0, filter_id = id.0, %err, "failed to remove stale FLOW_CONTROL_FILTER");
            }
        }
    }

    /// Promotes `new_table` to `handle`'s Active UniqueRespIdTable and
    /// reconciles ISO15765 `FLOW_CONTROL_FILTER`s against hardware: stops
    /// filters derived from the OLD Active table, installs filters derived
    /// from `new_table` (same per-entry derivation `SetUniqueRespIdTable` used
    /// before this promotion moved, ADR-039), then re-syncs the
    /// dual-channel-mode UUDT companion channel (ADR-046) against the new
    /// Active table. No pass-all `FLOW_CONTROL_FILTER` fallback is ever
    /// installed here or anywhere else in this service (ADR-048 dropped it
    /// at `ConnectComLogicalLink`; ADR-122 dropped it here and at
    /// `CLEAR_MSG_FILTERS` too — the zero-mask fallback this comment used to
    /// describe was spec-non-conformant and has been removed entirely).
    ///
    /// Skips ALL filter/companion-channel I/O when `new_table` is
    /// element-wise equal (order-insensitive by `unique_resp_identifier`) to
    /// the current Active table -- e.g. a `CoptUpdateparam` that only changed
    /// plain ComParam values does no filter churn (ADR-068).
    ///
    /// Called from `CoptUpdateparam` execution (`events::handle_update_param`).
    /// `ConnectComLogicalLink` does NOT call this: it promotes and installs
    /// filters directly (ADR-048).
    ///
    /// `Ok(())` (a no-op) if `handle` no longer exists (CLL destroyed
    /// concurrently).
    ///
    /// ADR-162 Decision 2: rejects (`Err(NativeMixedTableCollision)`, nothing
    /// written -- old table and filters stay installed) when `new_table`
    /// would introduce a native-mixed-mode `CP_CanRespUUDTId`/
    /// `CP_CanRespUSDTId` match-key collision, pooled against every sibling
    /// CLL's table already active on the same physical channel. The
    /// native-mixed gate (`effective_can_channel_mode()`) is read BEFORE
    /// `logical_links` is acquired (a new AB-BA rule alongside the existing
    /// `api`/`device_id` one -- `probe_can_channel_mode` holds
    /// `resolved_can_channel_mode` while awaiting `logical_links` via
    /// `ensure_uudt_companion_channel`, mirroring
    /// `install_point_to_point_fc_filters`'s identical note for
    /// `dual_channel`/`native_mixed`). The check, the sibling scan, and the
    /// `active_unique_resp_id_table` commit all happen in ONE `logical_links`
    /// critical section (merged from what used to be two separate early lock
    /// acquisitions), so a collision is checked strictly before
    /// `remove_point_to_point_fc_filters` ever runs.
    pub(super) async fn promote_unique_resp_id_table(
        &self,
        handle: u32,
        new_table: Vec<EcuUniqueRespEntry>,
    ) -> Result<(), NativeMixedTableCollision> {
        let native_mixed_mode =
            self.effective_can_channel_mode().await == CanChannelMode::NativeMixed;

        let (
            protocol,
            base_hw_protocol_id,
            hw_protocol_id,
            link_pin_select,
            link_channel_index,
            channel_id,
            old_filter_ids,
        ) = {
            let mut links = self.logical_links.lock().await;
            let Some(link) = links.get(&handle) else {
                return Ok(());
            };
            let protocol = link.protocol;
            // ADR-157 Plane B: the FC-filter rebuild gate below is a
            // protocol-family decision, so it must see the base protocol id
            // even for a `_PS`/`_CHx` link.
            let base_hw_protocol_id = link.base_hw_protocol_id();
            // ADR-157 Plane A: the filter messages themselves must carry the
            // actual connect-time (raw, possibly `_PS`/`_CHx`) protocol id
            // (found by Codex review, PR #28).
            let hw_protocol_id = link.hw_protocol_id;
            let link_pin_select = link.pin_select;
            let link_channel_index = link.channel_index;
            let channel_id = link.channel_id;
            let old_table = link.active_unique_resp_id_table.clone();

            if unique_resp_id_tables_equal(&old_table, &new_table) {
                // Content is unchanged: still store this exact snapshot as
                // Active (cheap, and keeps Active as the promoted instance),
                // but no filter/companion-channel I/O is warranted. An
                // already-accepted table can't newly collide, so this path
                // needs no collision check.
                if let Some(link) = links.get_mut(&handle) {
                    link.active_unique_resp_id_table = new_table;
                }
                return Ok(());
            }

            let enforce = native_mixed_mode
                && base_hw_protocol_id == j2534_0404::ISO15765
                && link_pin_select.is_none()
                && link_channel_index.is_none()
                && !resources::is_fd_protocol_id(hw_protocol_id);

            if enforce {
                let sibling_tables: Vec<Vec<EcuUniqueRespEntry>> = links
                    .iter()
                    .filter(|&(&h, l)| {
                        h != handle && channel_id.is_some() && l.channel_id == channel_id
                    })
                    .map(|(_, l)| l.active_unique_resp_id_table.clone())
                    .collect();
                let mut tables: Vec<&[EcuUniqueRespEntry]> = vec![new_table.as_slice()];
                tables.extend(sibling_tables.iter().map(Vec::as_slice));
                if let Some(collision) = find_native_mixed_uudt_usdt_collision(&tables) {
                    let collision = NativeMixedTableCollision(collision);
                    warn!(
                        cll_handle = handle,
                        collision = ?collision.0,
                        "promote_unique_resp_id_table rejected: native-mixed CAN mode \
                         CP_CanRespUUDTId/CP_CanRespUSDTId match-key collision",
                    );
                    return Err(collision);
                }
            }

            let old_filter_ids = if let Some(link) = links.get_mut(&handle) {
                link.active_unique_resp_id_table = new_table.clone();
                link.unique_resp_filter_ids.clone()
            } else {
                Vec::new()
            };

            (
                protocol,
                base_hw_protocol_id,
                hw_protocol_id,
                link_pin_select,
                link_channel_index,
                channel_id,
                old_filter_ids,
            )
        };

        let has_uudt_ids = new_table.iter().any(|e| uudt_resp_id(e).is_some());

        if base_hw_protocol_id == j2534_0404::ISO15765
            && let Some(channel_id) = channel_id
        {
            self.remove_point_to_point_fc_filters(channel_id, &old_filter_ids)
                .await;
            let new_filter_ids = self
                .install_point_to_point_fc_filters(
                    channel_id,
                    &new_table,
                    hw_protocol_id,
                    link_pin_select.is_some()
                        || link_channel_index.is_some()
                        || resources::is_fd_protocol_id(hw_protocol_id),
                )
                .await;
            let mut links = self.logical_links.lock().await;
            if let Some(link) = links.get_mut(&handle) {
                link.unique_resp_filter_ids = new_filter_ids;
            }
        }

        // ADR-157 accepted residual (Codex review, PR #28), widened to
        // `_CHx` by ADR-156 Decision 3 addendum/Phase 2b: dual-channel
        // mode's UUDT companion channel is always plain (unqualified) raw
        // CAN -- skipped here too, mirroring
        // `rpc_connect_com_logical_link`'s own qualifier guard on this same
        // mechanism, so a qualified link's post-connect UniqueRespIdTable
        // update can't open (or release) a companion on the wrong resource
        // either.
        if self.effective_can_channel_mode().await == CanChannelMode::DualChannel
            && CanChannelMode::applies_to(protocol)
            && link_pin_select.is_none()
            && link_channel_index.is_none()
            && !resources::is_fd_protocol_id(hw_protocol_id)
        {
            if has_uudt_ids {
                if let Err(status) = self.ensure_uudt_companion_channel(handle).await {
                    warn!(cll_handle = handle, %status, "failed to open UUDT companion CAN channel");
                }
            } else {
                self.release_uudt_companion_channel(handle).await;
            }
        }

        Ok(())
    }

    /// ADR-161 (rollback sub-rule): compare-and-restore for a shared-channel
    /// join that rolled back after bumping `ref_count`/stamping a fresh
    /// `occupancy_epoch` but before publishing -- destroying the joining CLL
    /// before that publish must not leave the channel's epoch permanently
    /// elevated when no new occupant actually survived. `rollback` carries
    /// this join's own `(stamped_epoch, prev_epoch)` pair; the restore to
    /// `prev_epoch` only happens if `sc.occupancy_epoch` still equals this
    /// join's own `stamped_epoch` -- i.e. no OTHER join has stamped a newer
    /// epoch in the gap between this join's stamp and its own rollback. A
    /// blind (non-compared) restore would erase that other join's evidence,
    /// which is the exact unsafe-clear outcome this whole mechanism exists to
    /// prevent. Originally load-bearing specifically for the UUDT-companion
    /// path, whose stamp and rollback used to be two separate
    /// `shared_channels` critical sections with a real gap between them; a
    /// dedicated follow-up later gave that path the same continuously-held
    /// guard the primary-connect path always had (see ADR-161's Correction
    /// note), so neither path strictly needs this anymore -- kept applied
    /// uniformly at both as defense-in-depth rather than special-cased away.
    /// `rollback: None` (every ordinary, non-rollback decrement) is a no-op.
    pub(super) fn restore_occupancy_epoch_on_rollback(
        sc: &mut SharedChannel,
        rollback: Option<(u64, u64)>,
    ) {
        if let Some((stamped_epoch, prev_epoch)) = rollback
            && sc.occupancy_epoch == stamped_epoch
        {
            sc.occupancy_epoch = prev_epoch;
        }
    }

    /// Decrements the ref count of the shared channel at `key`, disconnecting
    /// the physical channel when this was the last reference.  Disconnect
    /// failures are logged and otherwise ignored (the channel may already be
    /// gone after a hard error).
    ///
    /// `epoch_rollback` is `Some((stamped_epoch, prev_epoch))` only when this
    /// call is itself the rollback of a join that bumped `ref_count` and
    /// stamped `stamped_epoch` but never got to publish (ADR-161); every
    /// ordinary release call site passes `None`. See
    /// `restore_occupancy_epoch_on_rollback`'s doc comment for why this must
    /// be compare-and-restore, not a blind restore.
    pub(super) async fn release_shared_channel_ref(
        &self,
        key: ChannelKey,
        epoch_rollback: Option<(u64, u64)>,
    ) {
        let disconnected = {
            let mut chans = self.shared_channels.lock().await;
            if let Some(sc) = chans.get_mut(&key) {
                Self::restore_occupancy_epoch_on_rollback(sc, epoch_rollback);
                sc.ref_count -= 1;
                if sc.ref_count == 0 {
                    chans.remove(&key).map(|sc| sc.channel_id)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(channel_id) = disconnected {
            // ADR-101 Decision §E: hygiene, not correctness -- see the
            // sibling teardown sites' identical comment for why.
            self.drain_watermarks.lock().await.remove(&channel_id);
            let api = self.api.lock().await;
            if let Err(err) = api.disconnect(channel_id) {
                warn!(channel_id = channel_id.0, %err, "PassThruDisconnect failed for shared channel");
            }
        }
    }

    /// Dual-channel mode (ADR-046): opens (or joins) the companion raw-CAN
    /// channel used for UUDT reception on an ISO15765-family CLL.  The
    /// channel is keyed `(CAN, baud_rate)` in `shared_channels`, so it is
    /// shared with raw-CAN CLLs at the same baud rate and with other
    /// ISO15765 CLLs' companions.
    ///
    /// A no-op when the CLL is not connected (the companion is opened at
    /// `ConnectComLogicalLink` in that case) or already has a companion.
    /// The companion is receive-only from the service's perspective: the
    /// CLL's Working params are not applied to it beyond the baud rate.
    pub(super) async fn ensure_uudt_companion_channel(&self, handle: u32) -> Result<(), Status> {
        let (baud_rate, already_open, connected) = {
            let links = self.logical_links.lock().await;
            let link = links
                .get(&handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
            (
                link.channel_key.map(|(_, baud, _, _)| baud),
                link.uudt_channel_id.is_some(),
                link.connected,
            )
        };
        if already_open || !connected {
            return Ok(());
        }
        let Some(baud_rate) = baud_rate else {
            return Ok(());
        };

        let (device_guard, device_id) = self.ensure_open_device(handle).await?;

        // ADR-134: this is the single choke point for every companion-channel
        // open/join, reached not only from rpc_connect_com_logical_link's own
        // tail and probe_can_channel_mode but also from
        // promote_unique_resp_id_table (CoptUpdateparam execution on an
        // already-connected CLL) -- a path with no other module_state gate
        // anywhere in its call chain. Reject here, still holding
        // `device_guard`, if a hard channel error has marked this module
        // PduModstNotAvail, mirroring rpc_connect_com_logical_link's own
        // gate immediately above it in this file.
        // `module_state` is checked and released before `shared_channels` is
        // acquired below, the same lock order as
        // `rpc_connect_com_logical_link`'s identical pattern. The
        // `dead`-channel rejection below needs its own fresh `last_error`
        // read (it must not re-acquire `module_state` while holding
        // `shared_channels`), so no snapshot is carried out of this block.
        {
            let module_state = self.module_state.lock().await;
            if module_state.status != vci_service_interface::PduModuleStatus::PduModstReady {
                let (open_handle, _) =
                    (*device_guard).expect("ensure_open_device just confirmed a device is open");
                let last_error = module_state.last_error.clone();
                return Err(Self::module_not_avail_status(open_handle, last_error));
            }
        }

        // The UUDT companion is always a plain raw-CAN channel, never a
        // `_PS` link -- `pin_select` is always `0` here (ADR-156). It is
        // also never an FD link (FD+UUDT-companion-channel interaction is
        // structurally unreachable per ADR-158's Stage 3a scope), so the
        // fourth element (effective FD data-phase rate) is always `0` too.
        let channel_key: ChannelKey = (j2534_0404::CAN, baud_rate, 0, 0);

        // The companion channel is shared across CLLs by (CAN, baud) alone, and
        // each CLL's UniqueRespIdTable may configure either CAN-ID type for its
        // UUDT response -- heterogeneous and unknown at connect time -- so it
        // always connects CAN_ID_BOTH and gets a pass-all filter for each ID
        // type (ADR-065), rather than guessing a single type from whichever CLL
        // happens to trigger the open. Every raw-CAN primary channel now
        // connects CAN_ID_BOTH too (connect_flags's CAN case, ADR-065), so
        // reusing an already-open (CAN, baud) channel here -- whichever
        // creation order won the race, a raw-CAN CLL's primary or another
        // CLL's companion -- is always width-safe: the reused channel is
        // guaranteed to already be CAN_ID_BOTH.
        let connect_flags = j2534_0404::CAN_ID_BOTH;

        // Same TOCTOU-avoidance as rpc_connect_com_logical_link: hold the
        // shared_channels lock across the whole check-create-insert sequence.
        let mut chans = self.shared_channels.lock().await;
        let (channel_id, is_new_channel, epoch_rollback) = if let Some(sc) =
            chans.get_mut(&channel_key)
        {
            // ADR-134 Correction (Codex review round 6): same `dead`-channel
            // rejection as `rpc_connect_com_logical_link`'s identical join
            // branch -- see `SharedChannel::dead`'s doc comment.
            if sc.dead {
                let (open_handle, _) =
                    (*device_guard).expect("ensure_open_device just confirmed a device is open");
                // Read `last_error` fresh from `module_state`, immediately
                // before this rejection: `events::handle_channel_hard_error`
                // sets `sc.dead` under `shared_channels` before it updates
                // `module_state.last_error`/`status`, several `.await`s
                // later, so a snapshot taken before this critical section
                // could be stale for a connect landing in that window.
                // `shared_channels` is dropped first to preserve the
                // established device_id -> module_state -> shared_channels
                // lock order (never held simultaneously).
                drop(chans);
                let last_error = self.module_state.lock().await.last_error.clone();
                return Err(Self::module_not_avail_status(open_handle, last_error));
            }
            // Same reciprocal guard as rpc_connect_com_logical_link (Codex-review
            // fix): a raw-CAN CLL may have installed a PDU_IOCTL_START_MSG_FILTER
            // on this (CAN, baud) channel while it was still the sole owner. A
            // BLOCK_FILTER there would silently drop this UUDT companion's
            // frames if we joined anyway -- reject rather than let a second
            // consumer of the shared channel bypass the check the primary
            // connect path already enforces.
            let (filtered_by_another_cll, last_error) = {
                let links = self.logical_links.lock().await;
                let filtered = links
                    .values()
                    .any(|l| l.channel_id == Some(sc.channel_id) && !l.client_filters.is_empty());
                let last_error = links.get(&handle).and_then(|l| l.last_error.clone());
                (filtered, last_error)
            };
            if filtered_by_another_cll {
                // No PDUError variant means "function not supported" specifically
                // (that name never existed in the ISO 22900-2 PDUError enum this
                // service exposes); PDU_ERR_FCT_FAILED is the closest real code
                // for an adapter-level structural limitation like this one.
                return Err(state_guard_status(
                    Code::FailedPrecondition,
                    "PDU_ERR_FCT_FAILED: cannot join a physical channel that already \
                     has a PDU_IOCTL_START_MSG_FILTER filter installed by another ComLogicalLink",
                    PduError::PduErrFctFailed,
                    last_error,
                ));
            }
            sc.ref_count += 1;
            // ADR-161: re-stamp the occupancy epoch on this UUDT-companion
            // join too -- this join never touches `connect_generation`, so
            // the epoch is the only signal `ioctl_reset`'s Phase 1 has for
            // detecting it. `prev_occupancy_epoch` is captured before the
            // await/re-stamp so a rollback below (via `rollback_channel_join`,
            // reusing this same still-held `chans` guard -- see the ADR-161
            // Correction note) can restore it, compare-and-restore, if the
            // CLL is destroyed before this join publishes (see
            // `restore_occupancy_epoch_on_rollback`'s doc comment).
            let prev_occupancy_epoch = sc.occupancy_epoch;
            let stamped_occupancy_epoch = self.next_occupancy_epoch().await;
            sc.occupancy_epoch = stamped_occupancy_epoch;
            (
                sc.channel_id,
                false,
                Some((stamped_occupancy_epoch, prev_occupancy_epoch)),
            )
        } else {
            // last_error is read fresh at each failure point below, after `api`
            // is released, rather than snapshotted once up front -- see the
            // primary-channel connect path above (Codex review, ADR-105).
            let api = self.api.lock().await;
            let connect_result = api.connect(device_id, j2534_0404::CAN, connect_flags, baud_rate);
            let channel_id = match connect_result {
                Ok(channel_id) => channel_id,
                Err(err) => {
                    warn!(cll_handle = handle, baud_rate, connect_flags, %err, "PassThruConnect (UUDT companion CAN channel) failed");
                    drop(api);
                    let last_error = self
                        .logical_links
                        .lock()
                        .await
                        .get(&handle)
                        .and_then(|l| l.last_error.clone());
                    return Err(map_native_error_for_link(
                        "PassThruConnect for UUDT companion CAN channel",
                        &err,
                        last_error,
                    ));
                }
            };
            if let Err(err) =
                install_pass_all_filter(&api, channel_id, j2534_0404::CAN, connect_flags)
            {
                let _ = api.disconnect(channel_id);
                drop(api);
                let last_error = self
                    .logical_links
                    .lock()
                    .await
                    .get(&handle)
                    .and_then(|l| l.last_error.clone());
                return Err(map_native_error_for_link(
                    "PassThruStartMsgFilter",
                    &err,
                    last_error,
                ));
            }
            drop(api);
            (channel_id, true, None)
        };
        if is_new_channel {
            // ADR-178: this UUDT-companion path always connects with the
            // fixed `j2534_0404::CAN` protocol id above (never one of the 32
            // `PROTOCOL_ANALOG_IN_x` ids), so `applied_analog_sample_rate` is
            // always `None` here -- and, for the same reason (ADR-216
            // Decision item 10 fix), so are
            // `applied_analog_samples_per_reading`/`applied_analog_readings_
            // per_msg`. Same reasoning for `rx_supported` (ADR-194/
            // Phase 16): always CAN, never Ethernet_NDIS, so always `true`.
            self.spawn_new_shared_channel(
                channel_key,
                channel_id,
                connect_flags,
                None,
                None,
                None,
                true,
                &mut chans,
            )
            .await;
        }
        // `device_id` is unused past this point in this function: the tail
        // below only touches `shared_channels`/`logical_links`, not
        // `device_id`. Dropping here is both safe and sufficient -- no later
        // call in this function's control flow re-acquires `device_id`
        // (ADR-107 addendum (i), Codex review on PR #110).
        drop(device_guard);

        // ADR-161 (closing this function's own Prioritized Backlog entry):
        // `chans` (this function's own `shared_channels` guard) stays locked
        // into the `logical_links` publish below instead of being dropped
        // first, mirroring `finalize_connected_link`'s identical pattern for
        // the primary-channel join. Without this, a hard error landing in the
        // gap between the ref-count/occupancy-epoch bump above and
        // `uudt_channel_id` becoming visible here would let
        // `events::handle_channel_hard_error`'s `shared_channels`-held
        // occupant sweep miss this still-joining CLL entirely (that sweep
        // matches on `uudt_channel_id`, which isn't published yet), leaving
        // it publish a companion onto a channel already marked `dead`.
        let mut links = self.logical_links.lock().await;
        if let Some(link) = links.get_mut(&handle) {
            link.uudt_channel_id = Some(channel_id);
            link.uudt_channel_key = Some(channel_key);
            info!(
                cll_handle = handle,
                baud_rate, is_new_channel, "opened UUDT companion CAN channel"
            );
            drop(links);
            drop(chans);
            Ok(())
        } else {
            // CLL destroyed concurrently: roll back the ref bump / channel,
            // mirroring finalize_connected_link (ADR-012). `epoch_rollback`
            // carries this join's own `(stamped_epoch, prev_epoch)` pair so
            // the compare-and-restore in `rollback_channel_join` can undo the
            // re-stamp above without clobbering a sibling's newer one
            // (ADR-161). `chans` is still held here (per the ADR-161 fix
            // above), so this uses `rollback_channel_join`'s
            // already-locked-guard variant rather than
            // `release_shared_channel_ref`, which internally re-acquires
            // `shared_channels` and would self-deadlock.
            drop(links);
            self.rollback_channel_join(&mut chans, channel_key, epoch_rollback)
                .await;
            drop(chans);
            Err(unknown_handle_status(format!(
                "cll_handle {handle} was destroyed while the UUDT companion channel was being opened"
            )))
        }
    }

    /// Releases the CLL's UUDT companion channel reference, if any.
    pub(super) async fn release_uudt_companion_channel(&self, handle: u32) {
        let key = {
            let mut links = self.logical_links.lock().await;
            links.get_mut(&handle).and_then(|l| {
                l.uudt_channel_id = None;
                l.uudt_channel_key.take()
            })
        };
        if let Some(key) = key {
            self.release_shared_channel_ref(key, None).await;
        }
    }

    /// SAE J2534-2 clause 21 CAN FD (ADR-158) and clause 22
    /// ISO15765-on-CAN-FD (ADR-159) connect-time protocol substitution:
    /// infers FD mode from this CLL's staged Working ComParams
    /// (`CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate`) and substitutes
    /// `link.hw_protocol_id`/`base_hw_protocol_override` to/from
    /// `PROTOCOL_FD_CAN_PS`/`PROTOCOL_FD_ISO15765_PS` accordingly, dispatched
    /// on the link's base hardware protocol family. Mirrors
    /// [`Self::autodetect_sae_j1850_flavor`]'s shape (mutate
    /// `link.hw_protocol_id`/`base_hw_protocol_override` under
    /// `logical_links`, then `recompute_lock_tx_suspensions` in the SAME
    /// critical section per ADR-123 Finding G, then wake any resumed
    /// sibling) -- called right alongside it in
    /// `rpc_connect_com_logical_link`, before that function's own snapshot
    /// block reads either field.
    ///
    /// A no-op for every link whose base hardware protocol id (ADR-157 Plane
    /// B) is neither `CAN` nor `ISO15765` AND whose Working ComParams don't
    /// signal FD. **ADR-158 correction (Codex review, PR #30):** a link
    /// outside those two families whose Working ComParams DO signal FD is an
    /// error, not a no-op -- `is_param_allowed`'s CAN-family gate covers
    /// `CAN` and `ISO15765` together, so a client can stage
    /// `CP_CANFDTxMaxDataLength`/`CP_CANFDBaudrate` on any CAN-family CLL;
    /// silently ignoring the request on an out-of-scope family (as opposed
    /// to rejecting it, this function's own established pattern for every
    /// other out-of-scope FD combination) would let `ConnectComLogicalLink`
    /// proceed as a classic channel with no indication the FD request was
    /// dropped. Mode is recomputed fresh on every connect attempt, never
    /// sticky: a link previously substituted to FD reverts to plain CAN/
    /// ISO15765 (or their own `_PS` variant, if a genuine Pin Selection is
    /// still in effect, or their own `_CHx` variant, if a SAE J2534-2 clause
    /// 7 Additional Channel index is still in effect -- ADR-213) the moment
    /// its Working set no longer signals FD.
    ///
    /// The trigger (`fd_mode_staged`, see its own doc comment) is `TX_DL > 8
    /// || baudrate != 0` (ADR-158): `TX_DL == 8` alone is Classic CAN's own
    /// max payload and therefore ambiguous, but a nonzero `CP_CANFDBaudrate`
    /// has no purpose except signaling FD mode (baudrate `0` is itself a
    /// valid/default value meaning "use `CP_Baudrate`", so "TX_DL > 8 AND a
    /// valid baudrate" would be unsatisfiable by default), so a nonzero
    /// baudrate is decisive on its own regardless of `TX_DL`. `rpc_primitive`'s
    /// `CoptUpdateparam` guard (ADR-158 correction, round 3) reuses this same
    /// trigger against the CLL's about-to-be-promoted Working snapshot.
    ///
    /// Returns `Err` (before any mutation) when FD mode is requested but
    /// this CLL cannot use it this phase: the link's base hardware protocol
    /// is neither `CAN` nor `ISO15765` (ADR-158 correction above -- checked
    /// unconditionally, since a link outside those two families is never
    /// `already_fd` and so is always a would-be new transition), or the
    /// connecting module has not opted into SAE J2534-2 (clause 5). A
    /// `CAN`-family link is also
    /// rejected when it is in software-ISO-TP mode (no software-ISO-TP
    /// extension for FD-sized segmented messages this phase) -- this check
    /// has no ISO15765-branch counterpart, since a software-ISO-TP CLL's
    /// `hw_protocol_id` is always raw `CAN` (`can_mode.rs`) and so is always
    /// caught by the `CAN` branch instead. These checks are only
    /// (re-)checked while actually transitioning INTO FD mode --
    /// software-ISO-TP mode is immutable for a link's whole
    /// lifetime once created, and module opt-in is static configuration, so
    /// a link that already passed them on an earlier connect need not repeat
    /// the check on a later reconnect that stays in FD mode. `channel_index`
    /// is NOT a rejection reason (ADR-213 supersedes ADR-158 Decision item
    /// 1/ADR-159 Decision item 5's own `channel_index.is_some()` rejection
    /// arms): a `_CHx`-connected link staging FD ComParams promotes to the
    /// corresponding `FD_CAN_CH<n>`/`FD_ISO15765_CH<n>` id instead, via
    /// [`resources::fd_protocol_id_for_link`].
    pub(super) async fn apply_fd_mode(&self, handle: u32) -> Result<(), Status> {
        // ADR-107: this service opens at most one device at a time, and
        // `ConnectComLogicalLink` requires an already-`CreateComLogicalLink`-d
        // `cll_handle`, which itself already pinned a device open via
        // `ensure_open_device_for` -- so `self.device_id` is `Some` by
        // construction here. A `None` peek (a concurrent `ModuleDisconnect`
        // racing this call) is treated as "not opted in": conservative, and
        // in that same race `link` below will typically already be gone from
        // `logical_links` too, making this whole function a no-op regardless.
        let opted_in = {
            let slot = self.device_id.lock().await;
            slot.is_some_and(|(module_handle, _)| {
                discovery::is_j2534_2_opted_in(
                    self.modules[(module_handle - 1) as usize].pname.as_deref(),
                )
            })
        };

        let wake_targets: Vec<(u32, mpsc::UnboundedSender<TxItem>)> = {
            let chans = self.shared_channels.lock().await;
            let mut links = self.logical_links.lock().await;
            let Some(link) = links.get_mut(&handle) else {
                return Ok(());
            };

            // The FD trigger must be read regardless of protocol family
            // (Codex review, PR #30): `is_param_allowed`'s `is_can_family()`
            // gate covers `CAN` *and* `ISO15765` together, so a client can
            // successfully `SetComParam(CP_CANFDTxMaxDataLength/
            // CP_CANFDBaudrate, ...)` on a hardware-ISO15765 CLL even though
            // this function only ever substitutes a `CAN`-family link to FD.
            // An early return keyed on `base_protocol_id == CAN` -- before
            // ever computing `fd_mode` -- would silently ignore that staged
            // FD request on ISO15765 and let `ConnectComLogicalLink` proceed
            // as a classic channel, contradicting this function's own
            // established pattern for every other out-of-scope FD
            // combination (`channel_index`, `software_isotp` below): reject
            // outright rather than silently ignore.
            let fd_mode = fd_mode_staged(&link.working);

            match resources::base_protocol_id(link.hw_protocol_id) {
                j2534_0404::CAN => {
                    let already_fd = resources::is_fd_protocol_id(link.hw_protocol_id);

                    if fd_mode && !already_fd {
                        if !opted_in {
                            return Err(Status::invalid_argument(
                                "this ComLogicalLink's Working ComParams request SAE J2534-2 \
                                 clause 21 CAN FD mode (CP_CANFDTxMaxDataLength/\
                                 CP_CANFDBaudrate), which is a SAE J2534-2 feature -- this \
                                 module has not opted into J2534-2 (its pname lacks the \
                                 \"J2534-2:\" prefix, clause 5)",
                            ));
                        }
                        // ADR-164/Phase 4 (SAE J2534-2 clause 9 Single Wire
                        // CAN): `base_protocol_id`'s new SW arms now collapse
                        // an SW_CAN_PS link onto this same `CAN` match arm --
                        // without this check, staging FD ComParams on an SW
                        // link would silently substitute it to `FD_CAN_PS`
                        // via the plain-CAN path below, dropping the caller's
                        // SW intent entirely (the exact bug shape ADR-158
                        // already guards for `channel_index`/software-ISO-TP
                        // just below). SWCAN and CAN FD are mutually
                        // exclusive families this phase; reject rather than
                        // silently substitute.
                        // ADR-212: re-keyed from `is_sw_protocol_id` to
                        // `is_sw_family_protocol_id` so a `_CHx`-connected SW
                        // link (now reachable since SWCAN gained Additional
                        // Channels support) is still recognized and rejected
                        // here, rather than falling through and being
                        // silently promoted to `FD_CAN_PS`.
                        if resources::is_sw_family_protocol_id(link.hw_protocol_id) {
                            return Err(Status::invalid_argument(
                                "this ComLogicalLink's Working ComParams request SAE J2534-2 \
                                 clause 21 CAN FD mode, but this ComLogicalLink is a SAE \
                                 J2534-2 clause 9 Single Wire CAN (SW_CAN_PS) link -- CAN FD is \
                                 not supported over Single Wire CAN in this phase",
                            ));
                        }
                        // ADR-168/Phase 6 (SAE J2534-2 clause 20 Fault-Tolerant
                        // CAN): the FT_CAN_PS analog of the SWCAN-vs-FD
                        // rejection just above -- `base_protocol_id`'s new FT
                        // arms (needed so `comparam_id::to_j2534_config_id`
                        // treats an FT link as CAN-family for free, ADR-168
                        // Decision 5) collapse an FT_CAN_PS link onto this
                        // same `CAN` match arm exactly the way the SW arms
                        // did, so without this guard staging FD ComParams on
                        // an FT link would silently substitute it to
                        // `FD_CAN_PS` here, dropping the caller's FT intent.
                        // ADR-211: re-keyed from `is_ft_protocol_id` to
                        // `is_ft_family_protocol_id` so a `_CHx`-connected FT
                        // link (now reachable since FT gained Additional
                        // Channels support) is still recognized and rejected
                        // here, rather than falling through and being
                        // silently promoted to `FD_CAN_PS`.
                        if resources::is_ft_family_protocol_id(link.hw_protocol_id) {
                            return Err(Status::invalid_argument(
                                "this ComLogicalLink's Working ComParams request SAE J2534-2 \
                                 clause 21 CAN FD mode, but this ComLogicalLink is a SAE \
                                 J2534-2 clause 20 Fault-Tolerant CAN (FT_CAN_PS) link -- CAN FD \
                                 is not supported over Fault-Tolerant CAN in this phase",
                            ));
                        }
                        if link.software_isotp {
                            return Err(Status::invalid_argument(
                                "this ComLogicalLink's Working ComParams request SAE J2534-2 \
                                 clause 21 CAN FD mode, but this ComLogicalLink is configured \
                                 for software ISO-TP (can_channel_mode = \"software-isotp\", \
                                 ADR-046) -- CAN FD is not supported over the software ISO-TP \
                                 engine in this phase",
                            ));
                        }
                        // ADR-213: index-aware promotion -- a `_CHx`-connected
                        // link (`channel_index = Some(n)`) staging FD
                        // ComParams now promotes to `FD_CAN_CH<n>` instead of
                        // being rejected (the `channel_index.is_some()`
                        // rejection this replaced, ADR-158 Decision item 1,
                        // is superseded by ADR-213).
                        link.hw_protocol_id =
                            resources::fd_protocol_id_for_link(j2534_0404::CAN, link.channel_index)
                                .unwrap_or(link.hw_protocol_id);
                        link.base_hw_protocol_override = Some(j2534_0404::CAN);
                    } else if !fd_mode && already_fd {
                        // ADR-213: three-way revert, mirroring the SAE J1850
                        // VPW/PWM autodetect rewrite's own revert shape
                        // (`autodetect_sae_j1850_flavor`, above) -- pin-select
                        // takes priority, then a `_CHx` channel_index reverts
                        // to that channel's own `CAN_CH<n>` (not bare `CAN`),
                        // and only a link with neither reverts to bare `CAN`.
                        link.hw_protocol_id = match link.pin_select {
                            Some(_) => resources::ps_protocol_id(j2534_0404::CAN)
                                .unwrap_or(j2534_0404::CAN),
                            None => match link.channel_index {
                                Some(index) => resources::chx_protocol_id(j2534_0404::CAN, index)
                                    .unwrap_or(j2534_0404::CAN),
                                None => j2534_0404::CAN,
                            },
                        };
                        link.base_hw_protocol_override = (link.pin_select.is_some()
                            || link.channel_index.is_some())
                        .then_some(j2534_0404::CAN);
                    }
                }
                j2534_0404::ISO15765 => {
                    // A software-ISO-TP CLL's `hw_protocol_id` is always raw
                    // `CAN` regardless of its service-level protocol identity
                    // (`can_mode.rs`), so it can never reach this branch --
                    // it is caught by the `CAN` branch's own software-ISO-TP
                    // rejection above instead. This assert documents that
                    // instead of a redundant runtime check (ADR-159).
                    debug_assert!(!link.software_isotp);

                    let already_fd = resources::is_fd_protocol_id(link.hw_protocol_id);

                    if fd_mode && !already_fd {
                        if !opted_in {
                            return Err(Status::invalid_argument(
                                "this ComLogicalLink's Working ComParams request SAE J2534-2 \
                                 clause 22 ISO15765-on-CAN-FD mode \
                                 (CP_CANFDTxMaxDataLength/CP_CANFDBaudrate), which is a SAE \
                                 J2534-2 feature -- this module has not opted into J2534-2 (its \
                                 pname lacks the \"J2534-2:\" prefix, clause 5)",
                            ));
                        }
                        // ADR-164/Phase 4: the SW_ISO15765_PS analog of the
                        // `CAN` branch's own SWCAN-vs-FD rejection above --
                        // see that comment for the full rationale.
                        // ADR-212: re-keyed from `is_sw_protocol_id` to
                        // `is_sw_family_protocol_id`, same reasoning as the
                        // `CAN` branch's own SW guard above.
                        if resources::is_sw_family_protocol_id(link.hw_protocol_id) {
                            return Err(Status::invalid_argument(
                                "this ComLogicalLink's Working ComParams request SAE J2534-2 \
                                 clause 22 ISO15765-on-CAN-FD mode, but this ComLogicalLink is \
                                 a SAE J2534-2 clause 9 Single Wire CAN (SW_ISO15765_PS) link \
                                 -- ISO15765-on-CAN-FD is not supported over Single Wire CAN in \
                                 this phase",
                            ));
                        }
                        // ADR-168/Phase 6: the FT_ISO15765_PS analog of the
                        // `CAN` branch's own SWCAN/FTCAN-vs-FD rejections
                        // above -- see that comment for the full rationale.
                        // ADR-211: re-keyed from `is_ft_protocol_id` to
                        // `is_ft_family_protocol_id`, same reasoning as the
                        // `CAN` branch's own FT guard above.
                        if resources::is_ft_family_protocol_id(link.hw_protocol_id) {
                            return Err(Status::invalid_argument(
                                "this ComLogicalLink's Working ComParams request SAE J2534-2 \
                                 clause 22 ISO15765-on-CAN-FD mode, but this ComLogicalLink is \
                                 a SAE J2534-2 clause 20 Fault-Tolerant CAN (FT_ISO15765_PS) \
                                 link -- ISO15765-on-CAN-FD is not supported over Fault-Tolerant \
                                 CAN in this phase",
                            ));
                        }
                        // ADR-213: index-aware promotion -- see the `CAN`
                        // branch's own equivalent comment above (the
                        // `channel_index.is_some()` rejection this replaced,
                        // ADR-159 Decision item 5, is superseded by ADR-213).
                        link.hw_protocol_id = resources::fd_protocol_id_for_link(
                            j2534_0404::ISO15765,
                            link.channel_index,
                        )
                        .unwrap_or(link.hw_protocol_id);
                        link.base_hw_protocol_override = Some(j2534_0404::ISO15765);
                    } else if !fd_mode && already_fd {
                        // ADR-213: three-way revert -- see the `CAN` branch's
                        // own equivalent comment above.
                        link.hw_protocol_id = match link.pin_select {
                            Some(_) => resources::ps_protocol_id(j2534_0404::ISO15765)
                                .unwrap_or(j2534_0404::ISO15765),
                            None => match link.channel_index {
                                Some(index) => {
                                    resources::chx_protocol_id(j2534_0404::ISO15765, index)
                                        .unwrap_or(j2534_0404::ISO15765)
                                }
                                None => j2534_0404::ISO15765,
                            },
                        };
                        link.base_hw_protocol_override = (link.pin_select.is_some()
                            || link.channel_index.is_some())
                        .then_some(j2534_0404::ISO15765);
                    }
                }
                _ => {
                    if fd_mode {
                        return Err(Status::invalid_argument(
                            "this ComLogicalLink's Working ComParams request SAE J2534-2 FD \
                             mode (CP_CANFDTxMaxDataLength/CP_CANFDBaudrate), but this \
                             ComLogicalLink's base hardware protocol is neither CAN nor \
                             ISO15765 -- FD mode is only supported on those two families (SAE \
                             J2534-2 clause 21 CAN FD and clause 22 ISO15765-on-CAN-FD)",
                        ));
                    }
                    return Ok(());
                }
            }

            // ADR-123 Finding G: the recompute runs in the SAME critical
            // section as the write above, mirroring
            // `autodetect_sae_j1850_flavor`.
            let resumed = recompute_lock_tx_suspensions(&mut links);
            resumed
                .into_iter()
                .filter_map(|h| {
                    links
                        .get(&h)
                        .and_then(|l| l.channel_key)
                        .and_then(|ck| chans.get(&ck).map(|sc| (h, sc.tx_queue.clone())))
                })
                .collect()
        };
        for (h, tx_queue) in wake_targets {
            let _ = tx_queue.send(TxItem::ResumeWake { cll_handle: h });
        }
        Ok(())
    }

    /// `Auto` mode (ADR-046 addendum): probes, once per service instance,
    /// whether a companion raw-CAN channel can be opened alongside `handle`'s
    /// just-connected physical ISO15765 channel, and caches the outcome as
    /// the effective mode (`DualChannel` if the probe channel opened,
    /// `SingleChannel` if it did not) for every later `effective_can_channel_mode()`
    /// call. A no-op when `can_channel_mode` is not `Auto`, or when a prior
    /// probe (on this or another CLL) already resolved it.
    ///
    /// Reuses `ensure_uudt_companion_channel`/`release_uudt_companion_channel`
    /// for the actual open/close, so the probe channel is torn down
    /// immediately regardless of outcome — this call only decides policy, it
    /// never leaves a channel open on `handle`'s behalf. The
    /// `resolved_can_channel_mode` lock is held across the probe's `.await`
    /// points so concurrent connects from multiple CLLs cannot both probe.
    ///
    /// Epoch-tagged self-validating cache (ADR-107 addendum (h)): a cached
    /// entry is a hit only if its stored epoch matches the current
    /// `device_epoch`, otherwise it is treated as a miss and re-probed. The
    /// write-back is gated on BOTH a device still being open and `handle`'s
    /// CLL still existing in `logical_links`, checked atomically while
    /// transiently holding `device_id` (lock order: this cache's lock,
    /// already held from the read above, then `device_id`, then
    /// `logical_links`, matching the established hierarchy) -- if either
    /// check fails (device closed, or this CLL was torn down by a
    /// concurrent `ModuleDisconnect`), the conclusion (Dual OR Single) is
    /// discarded without being cached, so a stale/foreign result can never
    /// poison the next module's real probe.
    pub(super) async fn probe_can_channel_mode(&self, handle: u32) {
        if self.can_channel_mode != CanChannelMode::Auto {
            return;
        }
        let mut resolved = self.resolved_can_channel_mode.lock().await;
        let current_epoch = self.device_epoch.load(Ordering::SeqCst);
        if resolved
            .and_then(|(epoch, mode)| (epoch == current_epoch).then_some(mode))
            .is_some()
        {
            return;
        }

        let dual_capable = self.ensure_uudt_companion_channel(handle).await.is_ok();
        if dual_capable {
            self.release_uudt_companion_channel(handle).await;
        }

        let mode = if dual_capable {
            CanChannelMode::DualChannel
        } else {
            CanChannelMode::SingleChannel
        };

        // Write-back: only if a device is still open, that module is still
        // PduModstReady, AND `handle`'s CLL is still live, all checked
        // atomically under `device_id` (device_id -> module_state ->
        // logical_links, the established lock order) -- covers the
        // pre-existing "dead handle" case (a concurrent ModuleDisconnect
        // raced `ensure_uudt_companion_channel` above), the "device
        // closed/reopened under a different module since this probe
        // started" case, and (ADR-134) the narrow window where a hard
        // channel error marks the module NotAvail between
        // `ensure_uudt_companion_channel`'s own gate above passing and this
        // write-back running -- without this, a stale `SingleChannel`
        // conclusion could be cached for a module that is momentarily
        // NotAvail (self-healing on the next open would fix it via the
        // epoch bump regardless, but there is no reason to cache it at all).
        let slot = self.device_id.lock().await;
        let status_ready = slot.is_some() && {
            self.module_state.lock().await.status
                == vci_service_interface::PduModuleStatus::PduModstReady
        };
        if status_ready && self.logical_links.lock().await.contains_key(&handle) {
            let epoch = self.device_epoch.load(Ordering::SeqCst);
            info!(
                cll_handle = handle,
                dual_capable,
                ?mode,
                "resolved can_channel_mode=auto"
            );
            *resolved = Some((epoch, mode));
        }
        drop(slot);
    }

    /// `SAE_J1850` VPW/PWM auto-detect (ADR-070): resolves, once per module,
    /// whether the combined `SAE_J1850` bus (0x0307) is wired for VPW or PWM,
    /// and applies the result to `handle`'s CLL.
    ///
    /// A no-op for every protocol except the two bus-agnostic ones
    /// (`ChannelProtocol::needs_j1850_autodetect`), and a no-op once `handle`
    /// is already connected (nothing left to resolve for it). Otherwise:
    /// consults the module-wide `j1850_bus_flavor` cache, running
    /// [`Self::probe_sae_j1850_flavor`] to populate it on the first such CLL
    /// only (the cache lock is held across the probe's `.await` points so
    /// concurrent connects cannot both probe, mirroring
    /// `probe_can_channel_mode`). **Only a [`J1850ProbeOutcome::Conclusive`]
    /// result is written to the cache** (verification-pass fix): an
    /// inconclusive probe (`Fallback`, e.g. the passive J2190 listen seeing no
    /// traffic, or a probe timeout) still resolves *this* CLL to the VPW
    /// default, but leaves the cache empty so a later CLL that can actively
    /// probe (the OBD-capable resource, `ISO_15031_5_ON_SAE_J1850`) still gets
    /// the chance to run a real probe instead of permanently inheriting an
    /// unconfirmed guess. Every call -- fresh probe or cache hit -- writes the
    /// resolved native protocol ID into `handle`'s `hw_protocol_id`, and on a
    /// PWM result, merges `comparam_defaults::sae_j1850_pwm_override_params`
    /// into `handle`'s Working `ComParamSet` in place.
    ///
    /// Epoch-tagged self-validating cache (ADR-107 addendum (h)): a cached
    /// entry is a hit only if its stored epoch matches the current
    /// `device_epoch`, otherwise it is treated as a miss and re-probed. A
    /// conclusive write-back is gated on BOTH a device still being open and
    /// `handle`'s CLL still existing in `logical_links`, checked atomically
    /// while transiently holding `device_id` (lock order: this cache's lock,
    /// already held from the read above, then `device_id`, then
    /// `logical_links`) -- if either check fails, the conclusion is
    /// discarded without being cached.
    ///
    /// **Pin Selection (ADR-156/157 Bug B fix), widened to `_CHx` (ADR-156
    /// Decision 3 addendum/Phase 2b): the cache is bypassed entirely --
    /// both read and write -- whenever `handle`'s link carries EITHER
    /// connect-time qualifier (`pin_select.is_some() ||
    /// channel_index.is_some()`).** `j1850_bus_flavor` is a single
    /// module-wide slot, not keyed by either qualifier; a different pin
    /// assignment or vendor-connector channel index can mean a genuinely
    /// different physical bus segment (the same principle ADR-156 Decision
    /// 2/3 already applies to `ChannelKey`/hardware id), so a qualified
    /// probe's result must never satisfy a later unqualified connect's
    /// cache hit, nor may an unqualified result be assumed valid for a
    /// qualified connect. A qualified CLL therefore always runs its own
    /// live probe. The cache lock is still acquired and held across that
    /// probe (serializing concurrent connects on the shared probe
    /// hardware, same as the unqualified path); it is simply never read
    /// from or written to in this case.
    ///
    /// **Accepted residual (`_CHx`, ADR-156 Decision 3 addendum): the
    /// probe itself (`probe_sae_j1850_flavor`) is unchanged for a
    /// `channel_index`-qualified link -- it still only knows how to apply
    /// `pin_select` (`_PS`'s J1962-pin mechanism) to its candidate
    /// channels, never a vendor-connector `_CHx` index.** Clause 7's
    /// vendor-connector routing is opaque to this service (unlike clause
    /// 6's DLC pins), so there is no equivalent "probe on the caller's
    /// selected connector" mechanism to build here; a `_CHx`-qualified
    /// J1850 link's probe exercises the connect candidate's default pins,
    /// same as an unqualified probe would. The cache bypass above still
    /// applies (a `_CHx` link is a distinct physical resource, so its
    /// probe result -- however it was obtained -- must not pollute or be
    /// satisfied by the shared cache), and the write-back below still
    /// correctly threads the detected flavor back through the `_CHx`
    /// mapping so `hw_protocol_id` stays the connect-time `_CHx` variant.
    pub(super) async fn autodetect_sae_j1850_flavor(&self, handle: u32) {
        let (protocol, window_ms, pin_select, channel_index) = {
            let links = self.logical_links.lock().await;
            let Some(link) = links.get(&handle) else {
                return;
            };
            if link.connected || !link.protocol.needs_j1850_autodetect() {
                return;
            }
            (
                link.protocol,
                link.working.j1850_autodetect_window_ms(),
                link.pin_select,
                link.channel_index,
            )
        };
        let qualified = pin_select.is_some() || channel_index.is_some();

        let mut cached = self.j1850_bus_flavor.lock().await;
        let hit = if qualified {
            // Bug B fix, widened to `_CHx`: never read a cached (necessarily
            // unqualified) result for a qualified connect -- see the doc
            // comment above.
            None
        } else {
            cached.and_then(|(epoch, flavor)| {
                (epoch == self.device_epoch.load(Ordering::SeqCst)).then_some(flavor)
            })
        };
        let resolved = match hit {
            Some(flavor) => flavor,
            None => {
                let outcome = self
                    .probe_sae_j1850_flavor(handle, protocol, window_ms, pin_select)
                    .await;
                // Cache only a conclusive, unqualified result -- see the
                // doc comment above. A `Fallback`, or any qualified result,
                // leaves `*cached` untouched so the next CLL through here
                // (potentially one that can actively probe, or one on
                // different/no pins/a different channel index) still runs
                // `probe_sae_j1850_flavor` itself.
                if !qualified && let J1850ProbeOutcome::Conclusive(flavor) = outcome {
                    // Write-back: only if a device is still open, that
                    // module is still PduModstReady (ADR-134, symmetry with
                    // `probe_can_channel_mode`'s identical hardening -- not
                    // required for correctness, since a `Conclusive` result
                    // is real bus evidence regardless of module_state and
                    // the epoch tag already self-heals on the mandatory
                    // recovery's fresh `PassThruOpen`, but cheap to add),
                    // AND `handle`'s CLL is still live -- all checked
                    // atomically under `device_id` -- a concurrent
                    // ModuleDisconnect racing this probe must not have its
                    // stale conclusion cached against a device (or module)
                    // it no longer describes.
                    let slot = self.device_id.lock().await;
                    let status_ready = slot.is_some() && {
                        self.module_state.lock().await.status
                            == vci_service_interface::PduModuleStatus::PduModstReady
                    };
                    if status_ready && self.logical_links.lock().await.contains_key(&handle) {
                        let epoch = self.device_epoch.load(Ordering::SeqCst);
                        *cached = Some((epoch, flavor));
                    }
                    drop(slot);
                }
                outcome.flavor()
            }
        };
        drop(cached);

        let wake_targets: Vec<(u32, mpsc::UnboundedSender<TxItem>)> = {
            let chans = self.shared_channels.lock().await;
            let mut links = self.logical_links.lock().await;
            if let Some(link) = links.get_mut(&handle) {
                // ADR-156/Bug 2 fix, widened to `_CHx` (ADR-156 Decision 3
                // addendum): `resolved` is always a base `J1850VPW`/
                // `J1850PWM` id, never a `_PS`/`_CHx` variant. A qualified
                // link's `hw_protocol_id` was originally set to the `_PS`/
                // `_CHx` variant at `CreateComLogicalLink` time with its
                // qualifier field still `Some(_)`; assigning `resolved`
                // directly would silently revert it to the unqualified base
                // id while the qualifier field stays `Some`, so the connect
                // path would open a plain (unqualified) channel and then
                // illegally attempt `SET_CONFIG(CONFIG_J1962_PINS)` on it (a
                // `_PS` link -- rejected `ERR_CHANNEL_IN_USE` by the mock's
                // `pins_assigned` gating, since a non-`_PS` channel is
                // already considered to have its pins "assigned") or connect
                // the wrong vendor-connector channel entirely (a `_CHx`
                // link). Map back through `ps_protocol_id`/`chx_protocol_id`
                // (whichever qualifier is set) to preserve the variant.
                // Both `J1850VPW`/`J1850PWM` are always in ADR-156's 7
                // in-scope protocols and always have `_PS`/`_CHx` mappings,
                // so `unwrap_or(resolved)` is unreachable in practice --
                // kept instead of `.unwrap()`/`.expect()` on principle, to
                // avoid a panic if that invariant is ever violated.
                link.hw_protocol_id = if link.pin_select.is_some() {
                    resources::ps_protocol_id(resolved).unwrap_or(resolved)
                } else if let Some(index) = link.channel_index {
                    resources::chx_protocol_id(resolved, index).unwrap_or(resolved)
                } else {
                    resolved
                };
                // ADR-157 Bug C fix, widened to `_CHx`: `base_hw_protocol_override`
                // must move together with `hw_protocol_id` above -- it is
                // what `LogicalLinkState::base_hw_protocol_id()` actually
                // reads for every Plane B decision (ComParam support gating
                // among them), and it is set at `CreateComLogicalLink` time
                // to the *initial* candidate base id (always `J1850VPW`,
                // the default before autodetection runs). Leaving it stale
                // here would make `base_hw_protocol_id()` keep reporting
                // `J1850VPW` after a PWM detection, incorrectly rejecting
                // PWM-only ComParams (e.g. `CP_NetworkLine`) post-connect.
                if link.pin_select.is_some() || link.channel_index.is_some() {
                    link.base_hw_protocol_override = Some(resolved);
                }
                if resolved == j2534_0404::J1850PWM
                    && let Some(pwm_override) =
                        comparam_defaults::sae_j1850_pwm_override_params(protocol)
                {
                    link.working.unum32.extend(pwm_override.unum32);
                    link.working.bytes.extend(pwm_override.bytes);
                    link.working.structfield.extend(pwm_override.structfield);
                }
                // ADR-123 Finding G (Codex review round 3): hw_protocol_id is one of
                // the two fields `same_physical_resource` compares for the
                // pre-connect fallback match -- this write can change which
                // physical resource this not-yet-connected CLL belongs to, so the
                // recompute must run in the SAME critical section as the write
                // (same invariant as Fix A/E; see ADR-123 §3).
                let resumed = recompute_lock_tx_suspensions(&mut links);
                resumed
                    .into_iter()
                    .filter_map(|h| {
                        links
                            .get(&h)
                            .and_then(|l| l.channel_key)
                            .and_then(|ck| chans.get(&ck).map(|sc| (h, sc.tx_queue.clone())))
                    })
                    .collect()
            } else {
                Vec::new()
            }
        };
        for (h, tx_queue) in wake_targets {
            let _ = tx_queue.send(TxItem::ResumeWake { cll_handle: h });
        }
    }

    /// Sequential active-probe (ADR-070) deciding VPW vs. PWM for the
    /// `SAE_J1850` bus: `PassThruConnect(J1850VPW, 10.4k)`, install a pass-all
    /// filter (P1 fix -- see below), wait up to `window_ms` for a decoded
    /// response, and on silence, `PassThruDisconnect` and repeat on
    /// `J1850PWM, 41.6k`.
    ///
    /// For the OBD-active resource (`ISO_15031_5_ON_SAE_J1850`, 0x021C/
    /// 0x021D) each candidate first transmits a standard OBD-II functional
    /// Mode 01 PID 00 request (`J1850_OBD_PROBE_VPW`/`_PWM`) before reading;
    /// for the J2190 resource (`SAE_J2190_ON_SAE_J1850`, 0x021A) there is no
    /// universal probe request defined by SAE J2190, so this only listens
    /// passively -- a chatty ECU's own traffic is the only possible
    /// discriminator, and most quiescent buses fall through to the VPW
    /// default below.
    ///
    /// **Each candidate's channel needs its own pass-all filter (P1 fix)**,
    /// installed via [`install_pass_all_filter`] immediately after
    /// `PassThruConnect` and before writing/reading -- exactly like the real
    /// connect path below (`connect_new_physical_channel`), real J2534
    /// adapters silently discard every RX frame until at least one filter is
    /// installed. Without this, a real OBD ECU's probe response never
    /// reaches `PassThruReadMsgs` on real hardware, the read always times
    /// out, and auto-detect always falls back to (or, pre- the
    /// cache-only-conclusive fix, permanently locks onto) `J1850VPW`
    /// regardless of the bus's actual wiring. No explicit filter cleanup is
    /// needed: the unconditional `PassThruDisconnect` a few lines below tears
    /// the probe channel -- and every filter installed on it -- down before
    /// the next candidate (or the real connect) opens its own channel.
    ///
    /// Never fails: any hardware error (a candidate's `PassThruConnect`
    /// failing, the pass-all filter install failing, a probe
    /// `PassThruWriteMsgs` failing, or a `PassThruReadMsgs`
    /// timeout/empty-buffer) is treated as "no response" for that candidate
    /// and logged, so a probe malfunction cannot block the real connect that
    /// follows -- it only costs the VPW-default fallback used before this
    /// feature existed. Response/RX seen on either candidate wins outright
    /// and is [`J1850ProbeOutcome::Conclusive`]; neither conclusive defaults
    /// to VPW with a logged warning (preserving that previous fixed-VPW
    /// behavior on a quiescent bus) as [`J1850ProbeOutcome::Fallback`] -- see
    /// that type's doc comment for why the distinction matters to the caller.
    ///
    /// **Pin Selection (ADR-156/157 Bug B fix): `pin_select` is the
    /// connecting link's `LogicalLinkState.pin_select`.** When `Some(_)`,
    /// each candidate is opened via its `_PS` variant
    /// (`resources::ps_protocol_id(candidate_id)`) instead of the base id,
    /// and the caller's pins are bound with
    /// `PassThruIoctl(SET_CONFIG, CONFIG_J1962_PINS = pin_select)`
    /// immediately after `PassThruConnect` succeeds and before the pass-all
    /// filter/probe write -- otherwise the probe tests the bus on its
    /// default wiring instead of the caller's actually-requested pins, and a
    /// PWM bus reachable only via non-default pins is missed entirely. This
    /// mirrors `connect_new_physical_channel`'s identical connect ->
    /// `SET_CONFIG(CONFIG_J1962_PINS)` sequencing for the real connect that
    /// follows. A `SET_CONFIG` failure on the probe channel is treated the
    /// same as this function's other "never fails" hardware-error handling
    /// above: logged, the probe channel is disconnected, and the candidate
    /// counts as "no response" -- never propagated as a hard error, since a
    /// probe malfunction must never block the real connect that follows. The
    /// returned [`J1850ProbeOutcome`] always wraps a base (non-`_PS`)
    /// candidate id, exactly as in the non-pin-selected case: the caller
    /// (`autodetect_sae_j1850_flavor`) is the one that maps a pin-selected
    /// result back through `ps_protocol_id` for the real connect, and also
    /// the one responsible for bypassing the module-wide `j1850_bus_flavor`
    /// cache for a pin-selected probe (see that function's doc comment) --
    /// this function itself has no cache awareness.
    async fn probe_sae_j1850_flavor(
        &self,
        handle: u32,
        protocol: ChannelProtocol,
        window_ms: u32,
        pin_select: Option<u32>,
    ) -> J1850ProbeOutcome {
        let Ok((_device_guard, device_id)) = self.ensure_open_device(handle).await else {
            warn!(
                cll_handle = handle,
                "SAE_J1850 autodetect: could not open device for the probe, defaulting to VPW"
            );
            return J1850ProbeOutcome::Fallback(j2534_0404::J1850VPW);
        };

        // ADR-134 correction (Codex review, PR #149): this probe is NOT
        // read-only for the OBD-active resource -- it transmits a real OBD
        // Mode 01 PID 00 request on the vehicle bus (`active_probe` below).
        // Skip it entirely, still holding `_device_guard`, if a hard channel
        // error has marked this module NotAvail: `ConnectComLogicalLink`'s
        // own gate (this file, immediately after `autodetect_sae_j1850_flavor`
        // returns) will reject the connect anyway, so probing first would
        // transmit on the bus for a connect attempt that is already doomed.
        // Falls back the same way the "could not open device" branch above
        // does -- `Fallback` is never cached (see the caller's doc comment),
        // so this can't poison `j1850_bus_flavor` with a result no real
        // candidate was tried for.
        if self.module_state.lock().await.status
            != vci_service_interface::PduModuleStatus::PduModstReady
        {
            warn!(
                cll_handle = handle,
                "SAE_J1850 autodetect: module marked NotAvail, skipping the active probe and \
                 defaulting to VPW"
            );
            return J1850ProbeOutcome::Fallback(j2534_0404::J1850VPW);
        }

        let active_probe = protocol == ChannelProtocol::ISO_15031_5_ON_SAE_J1850;
        let candidates: [(u32, u32, &[u8]); 2] = [
            (j2534_0404::J1850VPW, 10_400, &J1850_OBD_PROBE_VPW),
            (j2534_0404::J1850PWM, 41_600, &J1850_OBD_PROBE_PWM),
        ];

        let api = self.api.lock().await;
        for (candidate_id, baud_rate, probe_frame) in candidates {
            // Bug B fix (ADR-156/157): a pin-selected connecting link must be
            // probed on its actually-requested pins, not the candidate's
            // default wiring -- open the `_PS` variant so `SET_CONFIG` below
            // is legal (a non-`_PS` channel's pins are already considered
            // "assigned"). `unwrap_or(candidate_id)` is unreachable in
            // practice (both J1850VPW/PWM always have a `_PS` mapping, ADR-156
            // Decision 2's Table 1), kept on principle rather than
            // `.unwrap()`/`.expect()` to avoid a panic if that invariant is
            // ever violated.
            let connect_id = if pin_select.is_some() {
                resources::ps_protocol_id(candidate_id).unwrap_or(candidate_id)
            } else {
                candidate_id
            };
            let channel_id = match api.connect(device_id, connect_id, 0, baud_rate) {
                Ok(id) => id,
                Err(err) => {
                    warn!(
                        cll_handle = handle,
                        candidate_id, connect_id, baud_rate, %err,
                        "SAE_J1850 autodetect: PassThruConnect failed for this candidate"
                    );
                    continue;
                }
            };

            // Bug B fix (ADR-156/157): bind the caller's selected pins right
            // after connect and before anything else on this channel --
            // mirrors `connect_new_physical_channel`'s identical connect ->
            // `SET_CONFIG(CONFIG_J1962_PINS)` sequencing for the real connect
            // that follows. A failure here is treated the same as every
            // other hardware error in this function: logged and counted as
            // "no response" for this candidate, never propagated -- a probe
            // malfunction must never block the real connect that follows.
            if let Some(pin_select) = pin_select
                && let Err(err) =
                    api.set_config_u32(channel_id, j2534_0404::CONFIG_J1962_PINS, pin_select)
            {
                warn!(
                    cll_handle = handle, candidate_id, connect_id, pin_select, %err,
                    "SAE_J1850 autodetect: probe PassThruIoctl SET_CONFIG (CONFIG_J1962_PINS) \
                     failed"
                );
                let _ = api.disconnect(channel_id);
                continue;
            }

            // P1 fix: real J2534 adapters silently discard every RX frame
            // until at least one filter is installed on the channel (see
            // `install_pass_all_filter`'s doc comment) -- without this, a
            // genuine OBD response on real hardware never reaches
            // `read_messages` below, the read times out, and this probe
            // always (wrongly) reports "no response". `connect_flags` is 0
            // (matching the `api.connect` call above), which only affects
            // `install_pass_all_filter`'s behavior for `protocol_id == CAN`
            // -- unreachable here (`connect_id` is always a J1850VPW/PWM
            // variant, `_PS` or not).
            // No explicit filter cleanup is needed: `api.disconnect` below
            // tears the channel, and every filter on it, down before the
            // next candidate (or the real connect) opens its own channel.
            if let Err(err) = install_pass_all_filter(&api, channel_id, connect_id, 0) {
                warn!(
                    cll_handle = handle, candidate_id, connect_id, %err,
                    "SAE_J1850 autodetect: probe PassThruStartMsgFilter failed"
                );
                let _ = api.disconnect(channel_id);
                continue;
            }

            if active_probe {
                // Plane A (hardware-facing, ADR-157): the message's
                // `ProtocolID` field must match whatever id the channel was
                // actually opened with (`connect_id`, `_PS` or not), not the
                // base `candidate_id`.
                match j2534_0404::PassThruMessage::new(connect_id, 0, 0, 0, 0, probe_frame) {
                    Ok(mut msg) => {
                        if let Err(err) = api.write_messages(
                            channel_id,
                            std::slice::from_mut(&mut msg),
                            window_ms,
                        ) {
                            warn!(
                                cll_handle = handle, candidate_id, connect_id, %err,
                                "SAE_J1850 autodetect: probe PassThruWriteMsgs failed"
                            );
                        }
                    }
                    Err(err) => warn!(
                        cll_handle = handle, candidate_id, connect_id, %err,
                        "SAE_J1850 autodetect: failed to build the probe message"
                    ),
                }
            }

            let saw_response = matches!(
                api.read_messages(channel_id, 1, window_ms),
                Ok(msgs) if !msgs.is_empty()
            );
            let _ = api.disconnect(channel_id);

            if saw_response {
                info!(
                    cll_handle = handle,
                    candidate_id, "SAE_J1850 autodetect: resolved"
                );
                return J1850ProbeOutcome::Conclusive(candidate_id);
            }
        }

        warn!(
            cll_handle = handle,
            "SAE_J1850 autodetect: no conclusive response on either flavor, defaulting to VPW"
        );
        J1850ProbeOutcome::Fallback(j2534_0404::J1850VPW)
    }

    /// Rebuilds every filter on an ISO15765 channel after `CLEAR_MSG_FILTERS`
    /// (which wipes all filters, including any point-to-point filters).
    /// Re-installs point-to-point filters for every CLL sharing the channel
    /// from its existing `active_unique_resp_id_table` (the table actually
    /// reflected by hardware before the clear). No pass-all fallback is
    /// re-installed for CLLs that still lack their own filter(s) -- that
    /// fallback was spec-non-conformant and has been removed entirely
    /// (ADR-122). See ADR-008, ADR-039.
    pub(super) async fn reinstall_iso15765_channel_filters_after_clear(
        &self,
        channel_id: ChannelId,
    ) {
        let cll_handles: Vec<u32> = {
            let links = self.logical_links.lock().await;
            links
                .iter()
                .filter(|(_, l)| l.channel_id == Some(channel_id))
                .map(|(&h, _)| h)
                .collect()
        };

        for handle in cll_handles {
            let (entries, hw_protocol_id, qualified) = {
                let links = self.logical_links.lock().await;
                links
                    .get(&handle)
                    .map(|l| {
                        (
                            l.active_unique_resp_id_table.clone(),
                            l.hw_protocol_id,
                            l.pin_select.is_some()
                                || l.channel_index.is_some()
                                || resources::is_fd_protocol_id(l.hw_protocol_id),
                        )
                    })
                    .unwrap_or_default()
            };
            let ids = self
                .install_point_to_point_fc_filters(channel_id, &entries, hw_protocol_id, qualified)
                .await;
            if let Some(link) = self.logical_links.lock().await.get_mut(&handle) {
                link.unique_resp_filter_ids = ids;
            }
        }
    }

    pub(super) async fn rpc_disconnect_com_logical_link(
        &self,
        request: Request<vci_service_interface::DisconnectComLogicalLinkRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;

        // Hold shared_channels across the whole filter-teardown + ref_count
        // sequence below (ADR-080: shared_channels is the outermost lock of
        // the three) -- otherwise a concurrent ConnectComLogicalLink's (or
        // ensure_uudt_companion_channel's) reciprocal client_filters check
        // could see this CLL's filters already drained below and join the
        // channel before the hardware filter is actually stopped, or before
        // ref_count reflects this CLL's departure. `logical_links` and `api`
        // are acquired and released underneath it, never before it (Codex-
        // review fix).
        let mut chans = self.shared_channels.lock().await;

        let (
            channel_key,
            channel_id_for_tp,
            client_filters,
            repeat_message_ids,
            hw_protocol_id,
            connect_generation,
            tp20_connection,
            tp20_broadcast_periodic,
            wake_targets,
        ) = {
            let mut links = self.logical_links.lock().await;
            let link = links
                .get_mut(&handle)
                .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
            let key = link.channel_key.take();
            let hw_protocol_id = link.hw_protocol_id;
            let connect_generation = link.connect_generation;
            // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c): take
            // this CLL's own live broadcast periodic message too, so its
            // best-effort stop below can run after this block releases the
            // `logical_links` lock -- mirrors `tp20_connection` just below.
            //
            // ADR-193 (Codex review round 15 Fix 2, P1, PR #101, corrected by
            // that round's own edge-case-hunter follow-up): this take is
            // deliberately here, in the SAME `logical_links` critical section
            // that clears `channel_id`/`channel_key`/`connected` -- NOT later,
            // under the `self.api` fence. Two reasons, both from ADR-193
            // Decision item 3:
            //
            // - Atomicity with this function's own session capture. Moving
            //   the take into the fenced section left a window in which this
            //   CLL was observable with `tp20_broadcast_periodic = Some(..)`
            //   but `channel_id`/`channel_key` already cleared, and any
            //   terminator that reads the entry under `logical_links` alone
            //   (`ioctl_suspend_tx_queue`, the `CP_SuspendQueueOnError`
            //   sites in `events.rs`, `CoptCancel`) would then capture
            //   `channel_id: None` alongside a live committed `message_id`,
            //   skip its native stop entirely, and report the COP terminal --
            //   leaving a still-transmitting native periodic with zero
            //   tracking anywhere. That is exactly the bug class ADR-193
            //   exists to close, so the take stays atomic with the clear.
            // - The take itself never needs the fence. It only removes an
            //   entry from `logical_links`; it touches no `primitives` state
            //   and reports no COP terminal. What needs the fence is the
            //   SUBSEQUENT native-stop/leak-track DECISION, which runs below
            //   under this function's own `api` guard -- the same shape
            //   `ioctl_suspend_tx_queue`/
            //   `terminate_tp20_broadcast_periodic_for_suspension` already
            //   use, and the reason `take_broadcast_periodic_under_api_locked`
            //   (`rpc_misc.rs`) is for id-targeted callers that have NOT yet
            //   taken the entry, not for a CLL-wide teardown like this one.
            let tp20_broadcast_periodic = link.tp20_broadcast_periodic.take();
            // SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a): take
            // this CLL's own connection state too, so its best-effort
            // teardown below can run after this block releases the
            // `logical_links` lock -- mirrors `hw_protocol_id`/
            // `connect_generation` just above.
            let tp20_connection = link.tp20_connection.take();
            // Tester-present (ADR-083, software-driven for both
            // `CP_TesterPresentSendType` values as of this diff) has no
            // hardware resource to release -- just clear the state.
            link.tester_present_state = TesterPresentState::None;
            // A full teardown clears any still-open discard windows too,
            // unlike a live reconfiguration (ADR-137 fourth Codex-review
            // fix / round-4 restructure): a disconnect ends this CLL's
            // session outright, so there is no future frame on this
            // physical channel that could still legitimately be a delayed
            // reply to a pre-disconnect tester-present send. Deliberately
            // NOT mirrored in `CoptStopcomm` (`handle_stop_comm`,
            // `events.rs`), which resets `tester_present_state` the same
            // way but must leave `open_tp_discards` untouched -- see that
            // function's own note.
            link.open_tp_discards.clear();
            let channel_id = link.channel_id;
            link.channel_id = None;
            link.connected = false;
            link.comm_started = false;
            link.stop_comm_pending = false;
            // Physical resource locks are tied to the connection; release them so other
            // CLLs are not permanently blocked while this CLL is offline.
            link.held_lock_mask = 0;
            // Take this CLL's client-installed message filters too, so they can be
            // stopped below regardless of whether the physical channel is shared or
            // about to close -- otherwise a filter installed via
            // PDU_IOCTL_START_MSG_FILTER would keep matching on hardware after this
            // CLL can no longer receive results for it.
            let filters: Vec<MessageFilterId> = link
                .client_filters
                .drain()
                .flat_map(|(_, ids)| ids)
                .collect();
            // SAE J2534-2 clause 14 Repeat Messaging (ADR-165 Decision 4):
            // take this CLL's live repeat-message MsgIds too, so they can be
            // stopped below the same way -- a repeat slot does not survive
            // its owning CLL's disconnect (J2534-1 §7.2.4's disconnect-stops-
            // all-periodic-messages requirement, extended by clause 14 to
            // repeat messages).
            let repeat_message_ids: Vec<u32> = std::mem::take(&mut link.repeat_message_ids);

            // ADR-123: this CLL's held physical-resource locks were just
            // released above -- recompute every CLL's tx_suspended_by_lock
            // and collect any sibling that transitioned to unsuspended, to
            // wake below (still inside the shared_channels-guarded critical
            // section, ADR-080).
            let resumed = recompute_lock_tx_suspensions(&mut links);
            let wake_targets: Vec<(u32, mpsc::UnboundedSender<TxItem>)> = resumed
                .into_iter()
                .filter_map(|h| {
                    links
                        .get(&h)
                        .and_then(|l| l.channel_key)
                        .and_then(|ck| chans.get(&ck).map(|sc| (h, sc.tx_queue.clone())))
                })
                .collect();

            (
                key,
                channel_id,
                filters,
                repeat_message_ids,
                hw_protocol_id,
                connect_generation,
                tp20_connection,
                tp20_broadcast_periodic,
                wake_targets,
            )
        };

        for (h, tx_queue) in wake_targets {
            let _ = tx_queue.send(TxItem::ResumeWake { cll_handle: h });
        }

        // Stop this CLL's client-installed message filters before it goes offline
        // (ADR-010's pattern): harmless if the channel is about to close anyway (PassThruDisconnect tears
        // down all filters with it), necessary when the channel is shared and stays
        // open for another CLL. A failed stop here is only logged, not retried or
        // re-tracked -- this is safe (not just best-effort) per ADR-082: a non-empty
        // `client_filters` implies this CLL is the *sole* owner of its channel (the
        // only ways to populate `client_filters` or to join an already-filtered
        // channel are mutually exclusive), so `client_filters` non-empty here always
        // means `ref_count` is about to hit 0 below and `PassThruDisconnect` tears
        // down every hardware filter on the channel regardless of this loop's outcome.
        if let Some(channel_id) = channel_id_for_tp {
            let api = self.api.lock().await;
            for filter_id in client_filters {
                if let Err(err) = api.stop_message_filter(channel_id, filter_id) {
                    warn!(cll_handle = handle, %err, "DisconnectComLogicalLink: stop_message_filter failed");
                }
            }
            // SAE J2534-2 clause 14 Repeat Messaging (ADR-165 Decision 4/6,
            // Codex review PR #42 round 2 Finding A): best-effort STOP for
            // every repeat slot this CLL started and never stopped --
            // mirrors the client-filter loop just above, but see
            // `DestroyComLogicalLink`'s identical (and more detailed)
            // comment for why a failed stop here is tracked, not just
            // logged, whenever the channel is NOT about to close. `chans` is
            // held from before this CLL's `channel_key` was captured through
            // the `ref_count` decrement below (see this function's own
            // lock-order comment), so this read is authoritative.
            let repeat_channel_will_stay_open = channel_key
                .and_then(|key| chans.get(&key))
                .is_some_and(|sc| sc.ref_count > 1);
            for msg_id in repeat_message_ids {
                if let Err(err) = api.stop_repeat_message(channel_id, msg_id) {
                    warn!(cll_handle = handle, msg_id, %err, "DisconnectComLogicalLink: stop_repeat_message failed");
                    if repeat_channel_will_stay_open
                        && let Some(sc) = channel_key.and_then(|key| chans.get_mut(&key))
                    {
                        sc.leaked_repeat_message_ids.push(msg_id);
                    }
                }
            }

            // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c):
            // best-effort stop of this CLL's own live broadcast periodic
            // message -- mirrors `DestroyComLogicalLink`'s identical
            // addition (see that function's own comment for why this is
            // deliberately UNCONDITIONAL, not gated on
            // `repeat_channel_will_stay_open`, and for Fix B's leak-tracking
            // mechanism this block also mirrors below).
            // Fix 2 (Codex review, P1, PR #101): mirrors
            // `DestroyComLogicalLink`'s identical `None`-sentinel skip just
            // above -- see that call site's own comment for the full
            // rationale.
            //
            // Fix B (design-advisor consult, Codex review round 3, ADR-192
            // Decision item 2): mirrors `DestroyComLogicalLink`'s identical
            // `leaked_periodic_message_ids` push -- a failed stop while a
            // sibling CLL keeps the physical channel open is tracked, not
            // just logged, exactly like `leaked_repeat_message_ids` above.
            //
            // ADR-193 (Codex review round 15 Fix 2, P1, PR #101, corrected by
            // that round's own edge-case-hunter follow-up): the entry itself
            // was already taken atomically with this CLL's `channel_id`/
            // `channel_key` clear, near the top of this function (see that
            // block's own comment for why the take must NOT be moved down
            // here). What ADR-193's fence actually protects is the DECISION
            // below -- whether a real `message_id` exists to stop natively --
            // and that runs here, under this function's own `api` guard,
            // which is the serialization point for an in-flight
            // `PassThruStartPeriodicMsg`. `channel_id`/`channel_key` are this
            // function's OWN pre-clear captures, per the "captured, not
            // re-derived" principle: re-reading them from `logical_links` at
            // this point would yield the already-cleared `None`s.
            if let Some(periodic) = tp20_broadcast_periodic
                && let Some(message_id) = periodic.message_id
                && let Err(err) = api.stop_periodic_message(channel_id, message_id)
            {
                warn!(
                    cll_handle = handle,
                    message_id = message_id.0,
                    %err,
                    "DisconnectComLogicalLink: best-effort PassThruStopPeriodicMsg for a TP2.0 \
                     broadcast periodic COP failed"
                );
                if repeat_channel_will_stay_open
                    && let Some(sc) = channel_key.and_then(|key| chans.get_mut(&key))
                    && !sc
                        .leaked_periodic_message_ids
                        .iter()
                        .any(|&(id, _)| id == message_id)
                {
                    sc.leaked_periodic_message_ids
                        .push((message_id, periodic.started_epoch));
                }
            }

            // SAE J2534-2 clause 16 SAE J1939 (ADR-179 Decision 3):
            // best-effort cancel of every address this CLL still owns in
            // its physical channel's `SharedChannel::j1939_claims`, mirroring
            // the repeat-message loop just above -- mirrors
            // `DestroyComLogicalLink`'s identical addition. Only while the
            // channel survives this CLL's own teardown -- `PassThruDisconnect`
            // below already tears down device state, including any claim,
            // when the channel is closing for good.
            if repeat_channel_will_stay_open
                && resources::is_j1939_protocol_id(hw_protocol_id)
                && let Some(sc) = channel_key.and_then(|key| chans.get_mut(&key))
            {
                events::cancel_j1939_claims_for_cll(
                    handle,
                    connect_generation,
                    channel_id,
                    &api,
                    sc,
                )
                .await;
            }

            // SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a):
            // best-effort teardown of this CLL's own connection, mirroring
            // `DestroyComLogicalLink`'s identical addition. Only while the
            // channel survives this CLL's own teardown -- `PassThruDisconnect`
            // below already tears down device state, including any
            // connection, when the channel is closing for good.
            // ADR-210 Decision item 11: re-keyed from the narrow
            // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id`,
            // mirroring `DestroyComLogicalLink`'s identical fix -- otherwise
            // a `_CHx`-connected TP2.0 CLL's own active connection would
            // never be torn down here, leaking the native connection slot.
            if repeat_channel_will_stay_open
                && resources::is_tp2_0_family_protocol_id(hw_protocol_id)
                && let Some(conn) = tp20_connection
                    .filter(|c| !c.passive && c.phase == Tp20ConnectionPhase::Established)
            {
                if let Err(err) = api.tp20_teardown_connection(channel_id, conn.requested_rx_id) {
                    warn!(
                        cll_handle = handle,
                        requested_rx_id = conn.requested_rx_id,
                        %err,
                        "DisconnectComLogicalLink: best-effort TP2.0 IOCTL_TEARDOWN_CONNECTION \
                         failed -- leaked native connection slot is an accepted residual \
                         (ADR-188 Consequences)"
                    );
                }
                // Codex review fix (PR #97, round 14, corrected round 17,
                // reverted round 21): quarantine rx_id -- see
                // `DestroyComLogicalLink`'s identical addition above and
                // `handle_stop_comm`'s (`events.rs`) for the full
                // round-21 rationale for why this is deliberately
                // UNCONDITIONAL, not gated on teardown success.
                if let Some(sc) = channel_key.and_then(|key| chans.get_mut(&key)) {
                    events::quarantine_tp20_connection_for_orphaned_write_back(
                        conn.requested_rx_id,
                        handle,
                        connect_generation,
                        &mut sc.tp20_connections,
                    );
                }
            }

            // ADR-190/Phase 7 Stage 7b section 4: passive disarm, mirroring
            // `DestroyComLogicalLink`'s identical addition above and
            // `handle_stop_comm`'s (`events.rs`) -- reverses
            // `arm_tp20_passive_listener` regardless of the listener's own
            // phase (`Listening` or `Established`), only while the channel
            // survives this CLL's own teardown.
            // ADR-210 Decision item 11: re-keyed from the narrow
            // `is_tp2_0_protocol_id` to `is_tp2_0_family_protocol_id`, the
            // same leak-closing fix as the active-connection arm above,
            // applied to the passive-listener-disarm path.
            if repeat_channel_will_stay_open
                && resources::is_tp2_0_family_protocol_id(hw_protocol_id)
                && let Some(conn) = tp20_connection.filter(|c| c.passive)
            {
                let rx_id_passive = conn.requested_rx_id;
                let was_established = conn.phase == Tp20ConnectionPhase::Established;
                let config_cleared =
                    events::best_effort_disarm_tp20_passive_native_config(&api, channel_id, handle);
                if was_established
                    && let Err(err) = api.tp20_teardown_connection(channel_id, rx_id_passive)
                {
                    warn!(
                        cll_handle = handle,
                        rx_id_passive,
                        %err,
                        "DisconnectComLogicalLink: best-effort TP2.0 IOCTL_TEARDOWN_CONNECTION \
                         on passive disarm failed -- leaked native connection slot is an \
                         accepted residual (ADR-190 Consequences), or the whole call is \
                         spec-ambiguous on a real adapter and self-heals via the maintenance \
                         timeout"
                    );
                }
                // `was_established`/`config_cleared` gate a bounded release
                // deadline for the never-`Established` disarm case (ADR-190's
                // Correction paragraph, Codex review finding, P1, PR #99) --
                // see `quarantine_tp20_passive_slot_on_disarm`'s own doc
                // comment.
                if let Some(sc) = channel_key.and_then(|key| chans.get_mut(&key)) {
                    events::quarantine_tp20_passive_slot_on_disarm(
                        rx_id_passive,
                        sc,
                        was_established,
                        config_cleared,
                    );
                }
            }
        }

        // Cancel any COPs that were queued but will never execute now that the link
        // is disconnecting.  Done before PduCllstOffline so events arrive in order.
        events::cancel_link_cops(
            &self.primitives,
            &self.logical_links,
            &self.subscriptions,
            &self.terminal_cops,
            handle,
        )
        .await;

        // ISO 22900-2's any-state-to-Offline use case: go directly to Offline from
        // any state — no intermediate Online step regardless of comm_started.
        events::send_cll_status(
            &self.subscriptions,
            &self.logical_links,
            handle,
            vci_service_interface::PduComLogicalLinkStatus::PduCllstOffline,
        )
        .await;

        if let Some(key) = channel_key {
            let disconnected = if let Some(sc) = chans.get_mut(&key) {
                sc.ref_count -= 1;
                if sc.ref_count == 0 {
                    chans.remove(&key).map(|sc| {
                        // SAE J2534-2 clause 14 Repeat Messaging backstop
                        // (ADR-165 Decision 6, Codex review PR #42 round 2
                        // Finding A): see `DestroyComLogicalLink`'s identical
                        // backstop comment -- the channel is closing for
                        // good, so any MsgIds still tracked as leaked here
                        // are dropped now rather than retried.
                        if !sc.leaked_repeat_message_ids.is_empty() {
                            debug!(
                                channel_id = sc.channel_id.0,
                                leaked_msg_ids = ?sc.leaked_repeat_message_ids,
                                "DisconnectComLogicalLink: dropping leaked repeat-message MsgIds, channel is closing"
                            );
                        }
                        // ADR-180 Decision 22: see `DestroyComLogicalLink`'s
                        // identical backstop comment above --
                        // `SharedChannel::leaked_j1939_claims`'s own field
                        // doc (`service.rs`) covers why any remaining entry
                        // is moot once the channel itself is gone.
                        if !sc.leaked_j1939_claims.is_empty() {
                            debug!(
                                channel_id = sc.channel_id.0,
                                leaked_addresses = ?sc.leaked_j1939_claims,
                                "DisconnectComLogicalLink: dropping leaked SAE J1939 claimed addresses, channel is closing"
                            );
                        }
                        // Fix B (design-advisor consult, Codex review round
                        // 3, ADR-192 Decision item 2): see
                        // `DestroyComLogicalLink`'s identical backstop
                        // comment above -- `SharedChannel::
                        // leaked_periodic_message_ids`'s own field doc
                        // (`service.rs`) covers why any remaining entry is
                        // moot once the channel itself is gone.
                        if !sc.leaked_periodic_message_ids.is_empty() {
                            debug!(
                                channel_id = sc.channel_id.0,
                                leaked_message_ids = ?sc.leaked_periodic_message_ids,
                                "DisconnectComLogicalLink: dropping leaked TP2.0 broadcast periodic \
                                 MessageIds, channel is closing"
                            );
                        }
                        sc.channel_id
                    })
                } else {
                    None
                }
            } else {
                None
            };
            drop(chans);
            if let Some(channel_id) = disconnected {
                // ADR-101 Decision §E: hygiene, not correctness -- see the
                // sibling teardown sites' identical comment for why.
                self.drain_watermarks.lock().await.remove(&channel_id);
                // last_error is read fresh here, after the native call fails, not
                // snapshotted at function entry: several await points (message
                // filter teardown, cancel_link_cops, send_cll_status) separate
                // entry from this call, any of which the poll task could race a
                // hard-error update into -- e.g. via this CLL's UUDT companion
                // channel, whose own hard-channel-error handling isn't blocked by
                // this function clearing only `channel_id`, not `uudt_channel_id`
                // (Codex review, edge-case-hunter follow-up, ADR-105).
                let result = self.api.lock().await.disconnect(channel_id);
                if let Err(err) = result {
                    let last_error = self
                        .logical_links
                        .lock()
                        .await
                        .get(&handle)
                        .and_then(|l| l.last_error.clone());
                    return Err(map_native_error_for_link(
                        "PassThruDisconnect",
                        &err,
                        last_error,
                    ));
                }
                // ADR-192/Phase 7 Stage 7c reinstated
                // `PassThruStartPeriodicMsg`/`PassThruStopPeriodicMsg` for
                // the TP2.0 broadcast-periodic-re-trigger case (superseding
                // this comment's prior ADR-083 claim that this service never
                // starts periodic messages) -- but `PassThruDisconnect`
                // above already tears down any live or leaked periodic
                // message on this channel along with the channel itself, so
                // there is still nothing further to stop here.
            }
            // Not the last CLL (physical channel stays open): tester-present
            // teardown for this CLL was already a plain state clear above --
            // no hardware call needed (ADR-083/this diff).
        } else {
            drop(chans);
        }

        // Release the dual-channel-mode UUDT companion channel, if any (ADR-046).
        self.release_uudt_companion_channel(handle).await;

        info!(cll_handle = handle, "DisconnectComLogicalLink");
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_lock_resource(
        &self,
        request: Request<vci_service_interface::LockResourceRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;
        let lock_mask = request.lock_mask;

        // §9.4.13.2 a) "validate all input parameters" (ADR-123, Fix 4):
        // Table D.2 defines only bits 0-1 (LOCK_PHYSICAL_COM_PARAMS /
        // LOCK_PHYSICAL_TX_QUEUE). Reject an all-zero mask and any mask
        // containing a bit outside those two -- even mixed with a defined
        // bit -- rather than silently narrowing it as before: a
        // silently-narrowed mask would let a client believe an undefined bit
        // got locked when it did not.
        if lock_mask == 0 || (lock_mask & !(LOCK_PHYSICAL_COM_PARAMS | LOCK_PHYSICAL_TX_QUEUE)) != 0
        {
            return Err(state_guard_status(
                Code::InvalidArgument,
                format!(
                    "PDU_ERR_INVALID_PARAMETERS: lock_mask {lock_mask:#04x} is zero or contains \
                     bits outside LOCK_PHYSICAL_COM_PARAMS | LOCK_PHYSICAL_TX_QUEUE",
                ),
                PduError::PduErrInvalidParameters,
                None,
            ));
        }

        // ADR-080: `shared_channels` is the outermost lock whenever more
        // than one of {shared_channels, logical_links, api} is held together
        // -- needed here because both the active-transmission check (Fix C,
        // ADR-123, below) and the post-grant resume-wake sends read a
        // `SharedChannel`. `mut` (Codex review, ADR-165 PR #42 round 11):
        // the leaked-repeat-message-id scan folded into the executing_cop
        // loop below needs `&mut Vec<u32>` to prune stale entries in place.
        let mut chans = self.shared_channels.lock().await;

        // ADR-110 amendment ("Lock-grant/apply serialization", Finding 2 --
        // Codex review on PR #116): `api` is acquired BEFORE `logical_links`
        // and held through the entire grant below, serializing this call
        // against `handle_update_param`'s own api-held lock-conflict
        // resolution + hardware push. Without this, a grant could snapshot
        // "not locked," return success to its caller, and only afterward
        // have a concurrent `CoptUpdateparam` -- which had already read a
        // stale `locked_by_other=false` before this grant ran -- push an
        // unfiltered ComParam set to hardware, clobbering exactly what the
        // grant was supposed to protect, with no `PDU_ERR_EVT_RSC_LOCKED`
        // event. Holding `api` first closes the window: either this grant
        // completes (and is visible) before `handle_update_param` reaches
        // its own api acquisition -- in which case its fresh
        // `find_physical_lock_holder` read sees the grant -- or it cannot
        // complete until after `handle_update_param` has released `api`,
        // i.e. after that push has already happened, so the grant only ever
        // protects from that point forward, never retroactively.
        // `rpc_unlock_resource` does not need this: releasing a lock while
        // an apply is in flight only makes an already-computed exclusion
        // conservative, never unsafe. New lock-ordering invariant: `api`
        // before `logical_links` whenever both are held together --
        // `logical_links` must never be held across an `api.lock().await`
        // acquisition (see ADR-110's amendment for the crate-wide sweep this
        // invariant was checked against). `shared_channels` is outermost of
        // all three (ADR-080/ADR-123).
        let api = self.api.lock().await;

        let mut links = self.logical_links.lock().await;
        let requesting = links
            .get(&handle)
            .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
        let (channel_key, hw_protocol_id, pin_select) = (
            requesting.channel_key,
            requesting.hw_protocol_id,
            requesting.pin_select,
        );

        // Reject if any OTHER CLL on the same physical resource already
        // holds any of the requested lock bits. PDU_ERR_RSC_LOCKED, not
        // _BY_OTHER_CLL (ADR-123): Table 21 (PDULockResource's own return
        // table, spec lines 1836-1845) does not list
        // RSC_LOCKED_BY_OTHER_CLL as a legal return here -- that code is
        // legal only for PDUUnlockResource (Table 22).
        if let Some((holder, held)) = find_physical_lock_holder(
            &links,
            handle,
            hw_protocol_id,
            pin_select,
            channel_key,
            lock_mask,
        ) {
            return Err(state_guard_status(
                Code::ResourceExhausted,
                format!(
                    "cll_handle {holder} already holds lock mask {held:#04x} on this physical resource",
                ),
                PduError::PduErrRscLocked,
                requesting.last_error.clone(),
            ));
        }

        // Fix C (ADR-123): LOCK_PHYSICAL_TX_QUEUE additionally requires no
        // CLL sharing this physical resource to have an ACTIVE,
        // actually-transmitting COP executing right now -- a
        // queued-but-not-yet-dispatched TxItem does not count, and a live but
        // non-transmitting executing COP (e.g. CoptDelay/CoptUpdateparam,
        // `CopEntry::transmits == false`, Fix D) does not block either. Not
        // applied when only LOCK_PHYSICAL_COM_PARAMS is requested: per spec
        // use case 2, a ComParam lock does not terminate/block an ongoing
        // transmission. Runs inside this grant's own `api`+`logical_links`
        // critical section (not a separate short-lived peek dropped before
        // `api`, as in the version this replaces), which closes the race
        // against `dispatch_tx_item`'s siphon-check-then-mark-executing: see
        // that function's comment (events.rs) for the interleaving argument.
        // Scans every channel matching this CLL's physical resource via
        // `same_physical_resource`, not just this CLL's own `channel_key`, so
        // a pre-connect LockResource (no `channel_key` yet) still finds an
        // already-connected sibling sharing the same `hw_protocol_id`.
        //
        // ADR-123 Finding J (Codex review round 5): this check is
        // resource-scoped, not CLL-scoped -- it does NOT exempt the
        // requesting CLL's own executing, transmitting COP. §9.4.13.2 b)
        // asks for a check of other locks plus active transmissions on the
        // resource; grammatically "other" qualifies only the locks part (a
        // CLL cannot lock-conflict with a lock it already holds), while the
        // active-transmissions part carries no such qualifier. A requester whose own
        // COP is mid-transmission is rejected exactly like any sibling's
        // would be; the busy window is transient/retryable, and a grant would
        // never suspend its own holder's traffic regardless.
        if lock_mask & LOCK_PHYSICAL_TX_QUEUE != 0 {
            let requester_last_error = links.get(&handle).and_then(|l| l.last_error.clone());
            for (ck, sc) in chans.iter_mut() {
                if !same_physical_resource(
                    (hw_protocol_id, pin_select, channel_key),
                    (ck.0, channel_key_pin_select(*ck), Some(*ck)),
                ) {
                    continue;
                }
                // Codex review, ADR-165 PR #42 round 11: a leaked repeat-
                // message slot (`SharedChannel::leaked_repeat_message_ids`,
                // service.rs) is a second, sibling occurrence of the exact
                // staleness/visibility gap round 6's fix
                // (`prune_stale_repeat_message_ids`, just below and in the
                // `repeat_message_ids` scan further down) already closed for
                // `LogicalLinkState::repeat_message_ids`. A slot is moved
                // here -- and dropped from `repeat_message_ids` entirely --
                // exactly when its owning CLL disconnects while a native
                // `STOP_REPEAT_MESSAGE` failed and a sibling CLL kept the
                // physical channel open (`ref_count > 0`): at that point
                // there is no `LogicalLinkState` left to scan via `links`,
                // so the per-link scan below cannot see it even though the
                // slot may still be autonomously transmitting on the device.
                // Scanned here (over `chans`, by physical-resource match,
                // like the `executing_cop` check immediately below) rather
                // than over `links`, since a leaked slot's ownership lives on
                // `SharedChannel`, not on any live `LogicalLinkState`.
                if !sc.leaked_repeat_message_ids.is_empty()
                    && Self::prune_stale_repeat_message_ids(
                        &api,
                        sc.channel_id,
                        &mut sc.leaked_repeat_message_ids,
                    )
                {
                    return Err(state_guard_status(
                        Code::FailedPrecondition,
                        "a leaked repeat-message slot on this physical resource's shared \
                         channel is actively transmitting"
                            .to_string(),
                        PduError::PduErrFctFailed,
                        requester_last_error,
                    ));
                }
                // SAE J2534-2 clause 19.3.2.3 (ADR-192/Phase 7 Stage 7c Fix
                // B, design-advisor consult, Codex review round 3): the
                // structural sibling of the `leaked_repeat_message_ids` scan
                // just above, one physical resource over --
                // `leaked_periodic_message_ids` (an orphaned-but-possibly-
                // still-live TP2.0 broadcast periodic from a prior failed
                // shared-channel-teardown/hard-error stop, `service.rs`)
                // keeps blocking new `LOCK_PHYSICAL_TX_QUEUE` grants exactly
                // like a live one would (ADR-192 Decision item 2). Retried/
                // pruned via `retry_leaked_periodic_message_stops`, which
                // IS the liveness probe here (no separate QUERY primitive
                // exists) -- see that function's own doc comment.
                if !sc.leaked_periodic_message_ids.is_empty()
                    && Self::retry_leaked_periodic_message_stops(
                        &api,
                        sc.channel_id,
                        &mut sc.leaked_periodic_message_ids,
                    )
                {
                    return Err(state_guard_status(
                        Code::FailedPrecondition,
                        "a leaked TP2.0 broadcast periodic message on this physical \
                         resource's shared channel is actively transmitting"
                            .to_string(),
                        PduError::PduErrFctFailed,
                        requester_last_error,
                    ));
                }
                if let Some(cop) = *sc.executing_cop.lock().await {
                    let owner = self
                        .primitives
                        .lock()
                        .await
                        .get(&cop)
                        .map(|e| (e.cll_handle, e.transmits));
                    // `executing_cop` and `primitives` are independently
                    // locked and updated non-atomically by the poll task: a
                    // COP can be removed from `primitives`
                    // (cancellation/staleness paths in events.rs) slightly
                    // before `dispatch_tx_item` clears `executing_cop`. Treat
                    // `owner == None` (no longer tracked) as "nothing to
                    // block on", not as "some CLL owns it" -- only a
                    // *resolved*, actually-transmitting owner blocks the
                    // grant (any CLL, including the requester itself -- Fix
                    // J above).
                    if let Some((_, transmits)) = owner
                        && transmits
                    {
                        // Table 21 offers only PDU_ERR_RSC_LOCKED or
                        // PDU_ERR_FCT_FAILED here; RSC_LOCKED's "already in
                        // the locked state" description is factually wrong
                        // for a busy-not-locked resource, so use FCT_FAILED.
                        return Err(state_guard_status(
                            Code::FailedPrecondition,
                            format!(
                                "cop_handle {cop} is actively transmitting on this physical resource",
                            ),
                            PduError::PduErrFctFailed,
                            links.get(&handle).and_then(|l| l.last_error.clone()),
                        ));
                    }
                }
            }

            // Finding L (Codex review, ADR-165 PR #42 round 5): a live
            // repeat-message slot (SAE J2534-2 clause 14) on a sibling CLL
            // sharing this physical resource is also an
            // actively-transmitting use of it, exactly like the
            // `executing_cop` scan above -- but Repeat Messaging is a
            // genuinely autonomous, device-driven TX stream that never goes
            // through `executing_cop`/`primitives` at all (ADR-165), so that
            // scan cannot see it. Scanned directly over `links` (like
            // `find_physical_lock_holder` does) rather than via `chans`,
            // since a repeat slot's ownership lives on `LogicalLinkState`,
            // not `SharedChannel`.
            //
            // Round 6 fix (Codex review, ADR-165 PR #42 round 6): this is
            // the third occurrence of the same staleness bug class (round 2
            // Finding C, round 3 Finding E) -- `repeat_message_ids`
            // membership is a claim, not a fact, since a `Condition == 1`
            // slot can self-complete device-side with zero notification back
            // to this service (ADR-165 Decision 1's thin-forwarder design).
            // A stale id left over from such a slot must not be allowed to
            // block this grant forever, so each remaining candidate's ids
            // are probed against the device (`prune_stale_repeat_message_ids`,
            // rpc_misc.rs) before the reject fires -- the id set is only
            // trusted once the ones the device disowns have been dropped.
            // Also removed the `other_handle == handle` self-skip: Fix J's
            // reading of ISO 22900-2 Section 9.4.13.2 b) two blocks above
            // (no "other" qualifier on the active-transmissions clause)
            // applies identically here -- a requester's own live repeat slot
            // is an active autonomous transmission on the resource and must
            // block its own grant exactly like a sibling's would.
            //
            // `requester_last_error` (round 11): reused from the `chans`
            // scan above rather than re-derived here -- nothing between the
            // two mutates `links.get(&handle).last_error`, so a second
            // snapshot would be redundant, not more correct.
            for (&other_handle, other_link) in links.iter_mut() {
                if other_link.repeat_message_ids.is_empty() {
                    continue;
                }
                if same_physical_resource(
                    (hw_protocol_id, pin_select, channel_key),
                    (
                        other_link.hw_protocol_id,
                        other_link.pin_select,
                        other_link.channel_key,
                    ),
                ) {
                    let Some(other_channel_id) = other_link.channel_id else {
                        // Should not happen -- ADR-165 Decision 4's teardown
                        // drain clears `repeat_message_ids` before a CLL
                        // disconnects. Defend anyway: an id set with no live
                        // channel to probe against cannot be pruned or
                        // trusted, so skip it rather than either blocking or
                        // silently discarding it.
                        debug!(
                            other_handle,
                            "LockResource: sibling has repeat_message_ids but no channel_id \
                             (should not happen -- ADR-165 teardown drain)"
                        );
                        continue;
                    };
                    if !Self::prune_stale_repeat_message_ids(
                        &api,
                        other_channel_id,
                        &mut other_link.repeat_message_ids,
                    ) {
                        continue;
                    }
                    return Err(state_guard_status(
                        Code::FailedPrecondition,
                        format!(
                            "a repeat-message slot on cll_handle {other_handle} is actively \
                             transmitting on this physical resource",
                        ),
                        PduError::PduErrFctFailed,
                        requester_last_error,
                    ));
                }
            }

            // Fix 3 (Codex review, P1, PR #101, ADR-192/Phase 7 Stage 7c): a
            // live TP2.0 broadcast periodic message is exactly the same
            // category as the repeat-message-slot scan just above -- an
            // autonomous, device-driven TX stream that never touches
            // `executing_cop`/`primitives` at all (started directly by
            // `rpc_start_com_primitive`, bypassing the ordinary tx_queue/
            // poll-task dispatch pipeline entirely, ADR-192 Decision item
            // 2) -- but was not checked anywhere in this function, so a CLL
            // could acquire `LOCK_PHYSICAL_TX_QUEUE` while a broadcast
            // periodic message kept transmitting on the shared physical
            // resource, defeating the lock's entire purpose. Scanned
            // directly over `links` (like the `repeat_message_ids` scan
            // above), since `tp20_broadcast_periodic` lives on
            // `LogicalLinkState`, not `SharedChannel`.
            //
            // No `other_handle == handle` self-skip, matching Fix J's own
            // reading of ISO 22900-2 Section 9.4.13.2 b) (the
            // active-transmissions clause has no "other" qualifier) and
            // Finding L's identical precedent for `repeat_message_ids`
            // above: a requester's own live broadcast periodic message is
            // an active autonomous transmission on the resource and must
            // block its own grant exactly like a sibling's would.
            //
            // A `None`-sentinel `message_id` reservation (Fix 2's
            // in-flight-start marker, `rpc_start_com_primitive`) counts as
            // blocking too: a reservation in flight is already "about to
            // transmit" -- the native `start_periodic_message` call just
            // hasn't returned yet -- so `tp20_broadcast_periodic.is_some()`
            // alone (not a nested message-id comparison) is the correct
            // test here.
            for (&other_handle, other_link) in links.iter() {
                if other_link.tp20_broadcast_periodic.is_none() {
                    continue;
                }
                if same_physical_resource(
                    (hw_protocol_id, pin_select, channel_key),
                    (
                        other_link.hw_protocol_id,
                        other_link.pin_select,
                        other_link.channel_key,
                    ),
                ) {
                    return Err(state_guard_status(
                        Code::FailedPrecondition,
                        format!(
                            "a TP2.0 broadcast periodic message on cll_handle {other_handle} is \
                             actively transmitting on this physical resource",
                        ),
                        PduError::PduErrFctFailed,
                        requester_last_error,
                    ));
                }
            }
        }

        links
            .get_mut(&handle)
            .expect("handle was just found in links above, and links has not been unlocked since")
            .held_lock_mask |= lock_mask;
        debug!(cll_handle = handle, lock_mask, "LockResource");

        // ADR-123: recompute every CLL's `tx_suspended_by_lock` now that this
        // grant may have newly suspended a sibling (spec use case 1) -- or,
        // in the fallback-vs-real-`channel_key` resource-match-change case
        // documented on `recompute_lock_tx_suspensions`, resumed one.
        let resumed = recompute_lock_tx_suspensions(&mut links);
        let wake_targets: Vec<(u32, mpsc::UnboundedSender<TxItem>)> = resumed
            .into_iter()
            .filter_map(|h| {
                links
                    .get(&h)
                    .and_then(|l| l.channel_key)
                    .and_then(|ck| chans.get(&ck).map(|sc| (h, sc.tx_queue.clone())))
            })
            .collect();
        drop(links);
        drop(api);

        for (h, tx_queue) in wake_targets {
            let _ = tx_queue.send(TxItem::ResumeWake { cll_handle: h });
        }
        // `chans` drops here at function end.
        Ok(Self::empty_response())
    }

    pub(super) async fn rpc_unlock_resource(
        &self,
        request: Request<vci_service_interface::UnlockResourceRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;
        let lock_mask = request.lock_mask;

        // §9.4.14.2 a) "validate all input parameters" (ADR-123, Fix 4): same
        // strict validation as `rpc_lock_resource` -- see its comment.
        if lock_mask == 0 || (lock_mask & !(LOCK_PHYSICAL_COM_PARAMS | LOCK_PHYSICAL_TX_QUEUE)) != 0
        {
            return Err(state_guard_status(
                Code::InvalidArgument,
                format!(
                    "PDU_ERR_INVALID_PARAMETERS: lock_mask {lock_mask:#04x} is zero or contains \
                     bits outside LOCK_PHYSICAL_COM_PARAMS | LOCK_PHYSICAL_TX_QUEUE",
                ),
                PduError::PduErrInvalidParameters,
                None,
            ));
        }

        // ADR-080: `shared_channels` is the outermost lock whenever more
        // than one of {shared_channels, logical_links} is held together --
        // needed here because the post-unlock resume-wake sends (ADR-123)
        // read a `SharedChannel`. No `api` acquisition on this path (see the
        // comment on `rpc_lock_resource`'s own `api` acquisition for why
        // unlock doesn't need it).
        let chans = self.shared_channels.lock().await;
        let mut links = self.logical_links.lock().await;
        let link = links
            .get_mut(&handle)
            .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;

        // Fix 1 (ADR-123): atomic, conflict-first validation -- mirrors
        // `rpc_lock_resource`'s existing atomic treatment of a multi-bit
        // mask (it already rejects the whole call if ANY requested bit
        // conflicts). A bit is held by at most one CLL at a time, so this
        // CLL's own held bits and "held by another CLL" are mutually
        // exclusive per-bit; checking the combined `leftover` mask in one
        // `find_physical_lock_holder` call is enough to decide which error
        // wins when the mask spans both states.
        let held_by_self = link.held_lock_mask & lock_mask;
        let leftover = lock_mask & !held_by_self;
        if leftover != 0 {
            let (channel_key, hw_protocol_id, pin_select) =
                (link.channel_key, link.hw_protocol_id, link.pin_select);
            let last_error = link.last_error.clone();
            if let Some((holder, held)) = find_physical_lock_holder(
                &links,
                handle,
                hw_protocol_id,
                pin_select,
                channel_key,
                leftover,
            ) {
                return Err(state_guard_status(
                    Code::ResourceExhausted,
                    format!(
                        "cll_handle {holder} already holds lock mask {held:#04x} on this \
                         physical resource -- spec 9.4.14.5: another ComLogicalLink holds a \
                         lock on the requested resource",
                    ),
                    PduError::PduErrRscLockedByOtherCll,
                    last_error,
                ));
            }
            return Err(state_guard_status(
                Code::FailedPrecondition,
                format!(
                    "lock_mask {leftover:#04x} is not held by cll_handle {handle} -- spec \
                     9.4.14.5: resource is already in the unlocked state",
                ),
                PduError::PduErrRscNotLocked,
                last_error,
            ));
        }

        link.held_lock_mask &= !lock_mask;
        debug!(cll_handle = handle, lock_mask, "UnlockResource");

        // ADR-123: recompute every CLL's `tx_suspended_by_lock` now that this
        // release may have unsuspended one or more siblings.
        let resumed = recompute_lock_tx_suspensions(&mut links);
        let wake_targets: Vec<(u32, mpsc::UnboundedSender<TxItem>)> = resumed
            .into_iter()
            .filter_map(|h| {
                links
                    .get(&h)
                    .and_then(|l| l.channel_key)
                    .and_then(|ck| chans.get(&ck).map(|sc| (h, sc.tx_queue.clone())))
            })
            .collect();
        drop(links);

        for (h, tx_queue) in wake_targets {
            let _ = tx_queue.send(TxItem::ResumeWake { cll_handle: h });
        }
        // `chans` drops here at function end.
        Ok(Self::empty_response())
    }

    /// Returns the Working-set value for `param_id`.
    ///
    /// `GetComParam` reads the Working set, not the hardware.  To read the
    /// value currently on the hardware, use the Active set (which equals the
    /// last Working set that was applied via `ConnectComLogicalLink` or
    /// `CoptUpdateparam`).
    pub(super) async fn rpc_get_com_param(
        &self,
        request: Request<vci_service_interface::GetComParamRequest>,
    ) -> Result<Response<vci_service_interface::ComParamResponse>, Status> {
        use vci_service_interface::get_com_param_request::Param;
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;
        let param = request
            .param
            .ok_or_else(|| Status::invalid_argument("param is required"))?;
        let link = self.get_link_state(handle).await?;
        let param_id = match param {
            Param::ParamId(id) => ComParamId(id),
            Param::ParamName(name) => Self::resolve_comparam_name(&name)?,
        };

        // The ComParam allowlist is hardware-flavor-dependent (e.g.
        // CP_NetworkLine is PWM-only, ADR-062), so it must key off the
        // link's actual connected hardware protocol, not its service-level
        // `ChannelProtocol` -- see `rpc_primitive::resolve_send_recv_tx`'s
        // `hw_protocol` for the same distinction on the TX path
        // (ADR-046/ADR-070/pin-typing amendment).
        comparam_support::check_param_allowed(
            ChannelProtocol::from_raw(link.base_hw_protocol_id()),
            param_id,
            link.last_error,
        )?;

        let param_data = if let Some(sf) = link.working.structfield.get(&param_id) {
            vci_service_interface::param_item::ParamData::Structfield(sf.clone())
        } else if let Some(bytes) = link.working.bytes.get(&param_id) {
            // Bytefield param (e.g. TESTER_PRESENT_MSG, J1939_NAME, CAN_BAUDRATE_RECORD).
            vci_service_interface::param_item::ParamData::Bytefield(bytes.clone())
        } else if comparam_support::is_bytefield_param(param_id) {
            // Bytefield-typed param with no seeded default for this protocol/bus
            // type (e.g. CP_CanBaudrateRecord on ISO_11898_3_DWFTCAN/SAE_J2411_SWCAN,
            // ADR-130; also e.g. CP_J1939Name on a non-J1939 CAN preset) -- report
            // the correct oneof variant, empty, rather than falling through to the
            // Unum32(0) default below (A2-18, ADR-133).
            vci_service_interface::param_item::ParamData::Bytefield(Vec::new())
        } else if comparam_support::is_structfield_param(param_id) {
            // Structfield-typed param with no seeded default for this protocol --
            // same reasoning as the Bytefield branch above (A2-18, ADR-133). The
            // two Structfield-typed params need different empty shapes; keep this
            // match exhaustive against comparam_support::STRUCTFIELD_PARAMS so a
            // future third structfield param can't silently fall through to the
            // Unum32 default this whole branch exists to avoid.
            let empty = match param_id {
                PARAM_SESSION_TIMING_OVERRIDE => comparam_defaults::session_timing_empty(),
                PARAM_EXTENDED_TIMING => comparam_defaults::access_timing_empty(),
                PARAM_ACCESS_TIMING_ECU => comparam_defaults::access_timing_empty(),
                PARAM_ACCESS_TIMING_OVERRIDE => comparam_defaults::access_timing_empty(),
                PARAM_SESSION_TIMING_ECU => comparam_defaults::session_timing_empty(),
                _ => unreachable!(
                    "comparam_support::STRUCTFIELD_PARAMS and this match must stay in sync"
                ),
            };
            vci_service_interface::param_item::ParamData::Structfield(empty)
        } else if param_id.is_vendor() && !link.working.unum32.contains_key(&param_id) {
            // ADR-219 Decision item 3: an unstaged vendor (`>= 0x10000`)
            // ComParamId's Working-read intentionally bypasses the generic
            // Unum32(0)-default fallback below -- this service has no
            // legitimate default for an id it does not itself recognize
            // (the same "no fabricated not-configured-but-looks-like-a-
            // value" principle ADR-133 already establishes for the
            // Bytefield/Structfield fallback above). Delegates to
            // `unstaged_vendor_config_read`: a live `PassThruGetConfig`
            // read when connected (WITHOUT staging the result into
            // `Working` -- a mere read must not silently mutate staged
            // state), else `FailedPrecondition`/`PDU_ERR_CLL_NOT_CONNECTED`
            // (no Working value and no live value to read either).
            let value = self.unstaged_vendor_config_read(handle, param_id).await?;
            vci_service_interface::param_item::ParamData::Unum32(value)
        } else {
            // Unum32 param; return 0 when not yet set (default) -- except
            // PARITY (0x16, CP_Parity), which has no explicit Working entry
            // when the client has only ever set CP_UartConfig (0x20): in
            // that case, derive the *effective* parity from the Working
            // CP_UartConfig entry via `uart_config_to_parity` instead of
            // reporting a stale `0`. Without this, a save/restore roundtrip
            // (GetComParam(0x20) + GetComParam(0x16), then SetComParam of
            // both) would silently corrupt the parity: the roundtripped
            // explicit PARITY=0 would then win over the CP_UartConfig-
            // derived parity in `expand_uart_config`, turning e.g.
            // CP_UartConfig=8 (8E1) into 8N1 (ADR-071). This mirrors
            // `expand_uart_config`'s write-side precedence in reverse: reads
            // still prefer an explicit 0x16 entry when present.
            //
            // The actual fallback logic now lives in
            // `comparam_support::effective_unum32`, shared with
            // `bustype_params_differ`/`tester_present_params_differ` so
            // "what does Get report for an unseeded/derived param" has
            // exactly one implementation (ADR-133 amendment, Codex review
            // round 2 on PR #147 -- a drift between this fallback and the
            // differ functions' comparison logic was the root cause of a
            // false-positive `PDU_ERR_TEMPPARAM_NOT_ALLOWED` on a normal
            // read-then-write-back round-trip of an unseeded/derived param).
            let v = comparam_support::effective_unum32(&link.working, param_id);
            // ADR-216 Decision item 8: CP_AnalogInputRangeLow/High report
            // signed at this GetComParam boundary specifically -- stored
            // internally in working.unum32 in their native two's-complement
            // 32-bit encoding (Table 16's own encoding), but surfaced via the
            // proto's existing, previously-unused `Snum32` oneof arm rather
            // than the `Unum32` arm every other numeric ComParam in this
            // service uses. The first signed-value-reporting ComParam in
            // this codebase; no corresponding SetComParam-time Snum32 write
            // path is needed, since both are read-only (rejected outright by
            // rpc_set_com_param's Unum32 arm before any value would ever
            // need parsing from that variant).
            if param_id == PARAM_ANALOG_INPUT_RANGE_LOW || param_id == PARAM_ANALOG_INPUT_RANGE_HIGH
            {
                vci_service_interface::param_item::ParamData::Snum32(v as i32)
            } else {
                vci_service_interface::param_item::ParamData::Unum32(v)
            }
        };

        let param_item = vci_service_interface::ParamItem {
            id: Some(vci_service_interface::param_item::Id::ParamId(param_id.0)),
            com_param_class: comparam_support::com_param_class(param_id) as i32,
            param_data: Some(param_data),
        };

        Ok(Response::new(vci_service_interface::ComParamResponse {
            param_item: Some(param_item),
        }))
    }

    /// ADR-219 Decision item 3's unstaged-vendor-ComParamId `GetComParam`
    /// read: reached only when `Working` holds no entry for `param_id`
    /// (`param_id.is_vendor()`, the caller's own gate). Reuses
    /// `resolve_live_legacy_link` (the same connected + live-`channel_id`
    /// resolution `rpc_io_ctl_vendor`'s handle resolution uses, ADR-080
    /// `shared_channels`-outermost discipline) to reject
    /// `FailedPrecondition`/`PDU_ERR_CLL_NOT_CONNECTED` when this CLL is not
    /// connected -- there is no live value to read either, and this
    /// service has no legitimate default to fabricate (ADR-133's
    /// "no fabricated not-configured-but-looks-like-a-value" principle).
    /// When connected, issues a live `PassThruGetConfig` read
    /// (`get_config_u32`) and returns that value directly -- the caller
    /// does NOT insert it into `Working` (a mere read must not silently
    /// mutate staged state).
    async fn unstaged_vendor_config_read(
        &self,
        cll_handle: u32,
        param_id: ComParamId,
    ) -> Result<u32, Status> {
        // `chans` (`shared_channels`) is deliberately kept held across the
        // native `get_config_u32` call below, only dropped once that call
        // returns -- not right after `resolve_live_legacy_link` resolves
        // `channel_id`. Dropping it early would open a TOCTOU window: a
        // concurrent disconnect/reconnect could let the native side recycle
        // this numeric channel id before the read actually runs, silently
        // reading a different, newly-connected channel's vendor config value
        // under the guise of this `cll_handle`'s request. `rpc_io_ctl_vendor`
        // (`rpc_misc.rs`)'s `CllHandle` branch already establishes this same
        // hold-across-the-native-call pattern for the identical race, via
        // `run_vendor_ioctl` -- mirrored here exactly, including acquiring
        // `self.api` while still holding `shared_channels` (an already
        // load-bearing lock-ordering convention in this codebase, not a new
        // one introduced here).
        let chans = self.shared_channels.lock().await;
        let (channel_id, _link) = self.resolve_live_legacy_link(&chans, cll_handle).await?;
        let result = self.api.lock().await.get_config_u32(channel_id, param_id.0);
        drop(chans);
        match result {
            Ok(value) => Ok(value),
            Err(err) => {
                let last_error = self
                    .logical_links
                    .lock()
                    .await
                    .get(&cll_handle)
                    .and_then(|l| l.last_error.clone());
                Err(map_native_error_for_link(
                    &format!("PassThruGetConfig vendor ComParam {:#010x}", param_id.0),
                    &err,
                    last_error,
                ))
            }
        }
    }

    /// Writes `param_item` into the Working param set.
    ///
    /// `SetComParam` always targets the Working set.  Values are not applied to
    /// the J2534 hardware until `ConnectComLogicalLink` (Offline → Online
    /// transition) or `CoptUpdateparam` (Working → Active).
    pub(super) async fn rpc_set_com_param(
        &self,
        request: Request<vci_service_interface::SetComParamRequest>,
    ) -> Result<Response<vci_service_interface::Response>, Status> {
        let request = request.into_inner();
        let handle = request
            .cll_handle
            .ok_or_else(|| Status::invalid_argument("cll_handle is required"))?
            .cll_handle;

        let item = request
            .param_item
            .ok_or_else(|| Status::invalid_argument("param_item is required"))?;
        let param_id = match item
            .id
            .ok_or_else(|| Status::invalid_argument("param_item.id is required"))?
        {
            vci_service_interface::param_item::Id::ParamId(id) => ComParamId(id),
            vci_service_interface::param_item::Id::ParamName(name) => {
                Self::resolve_comparam_name(&name)?
            }
        };

        match item.param_data {
            Some(vci_service_interface::param_item::ParamData::Unum32(value)) => {
                let mut links = self.logical_links.lock().await;

                let link = links
                    .get_mut(&handle)
                    .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
                // Hardware-flavor-dependent allowlist; see the doc comment
                // on the identical check in `rpc_get_com_param`.
                let protocol = ChannelProtocol::from_raw(link.base_hw_protocol_id());
                comparam_support::check_param_allowed(protocol, param_id, link.last_error.clone())?;
                // Every Bytefield-typed ComParam (CP_CanBaudrateRecord and any
                // future BUSTYPE_BYTES member, plus every other
                // BYTEFIELD_PARAMS/STRUCTFIELD_PARAMS member -- e.g.
                // CP_TesterPresentMessage, CP_J1939Name, CP_ExtendedTiming) must
                // be rejected here rather than accepted into `working.unum32`,
                // where `rpc_get_com_param`'s type-typed-but-unset fallback
                // branches (ADR-130's original CP_CanBaudrateRecord fix, now
                // generalized to every Bytefield/Structfield param by A2-18/
                // ADR-133) would otherwise silently mask the stored value behind
                // an empty Bytefield/Structfield response instead of ever
                // surfacing the type mismatch.
                if comparam_support::is_bytefield_param(param_id)
                    || comparam_support::is_structfield_param(param_id)
                {
                    return Err(Status::invalid_argument(format!(
                        "com_param_id {:#010x} is not a Unum32-typed ComParam; use the \
                         correct ParamData oneof variant (Bytefield or Structfield)",
                        param_id.0
                    )));
                }
                // CP_UartConfig (ADR-071): only the 6 encodings representable as a
                // J2534 v04.04 DATA_BITS/PARITY pair (no stop-bit param, no 9-data-bit
                // support) are accepted; everything else is rejected here rather than
                // silently truncated at forwarding time.
                if param_id == ComParamId(j2534_0404::DATA_BITS) && !is_valid_uart_config(value) {
                    return Err(Status::invalid_argument(format!(
                        "CP_UartConfig value {value} is not representable in J2534 v04.04 \
                         (accepted values: {UART_CONFIG_ACCEPTED:?})"
                    )));
                }
                // CP_Parity (J2534-specific alias, ADR-027): native PARITY range is 0..=2.
                if param_id == ComParamId(j2534_0404::PARITY) && !is_valid_parity(value) {
                    return Err(Status::invalid_argument(format!(
                        "CP_Parity value {value} is out of range (accepted: 0..=2)"
                    )));
                }
                // CP_BlockSizeOverride (direct-reuse mapping onto native BS_TX, names.rs):
                // native BS_TX accepts only 0x00..=0xFF plus the 0xFFFF sentinel ("use the
                // vehicle-reported flow-control value") -- reject anything in between here
                // rather than letting apply_params_to_hardware_locked's batched SET_CONFIG
                // silently fail (dropping every OTHER staged ComParam in the same batch
                // along with it). CP_BlockSizeOverride is a shared ComParamId allowed on
                // both CAN and ISO15765 links (comparam_support's allowlist), but
                // to_j2534_config_id only actually forwards it to native BS_TX on an
                // ISO15765 link -- on a plain CAN link it stays service-side in
                // `working.unum32` and never reaches SET_CONFIG, so the full
                // [0, 0xFFFF] range is legitimate there. Gate the range check on the
                // same to_j2534_config_id mapping apply_params_to_hardware_locked uses,
                // so the extra restriction only applies where the failure it prevents
                // can actually occur. Must pass the RAW `link.hw_protocol_id` here
                // (ADR-157 Plane A), not the normalized `base_hw_protocol_id()`
                // (Plane B): to_j2534_config_id's own contract (comparam_id.rs)
                // documents that it expects the raw, un-normalized id, since several
                // of its internal branches (the FD_ISO15765_PS three-param override,
                // SW/Honda/J1939 raw-id checks) are deliberately checked before
                // `resources::base_protocol_id` normalization and rely on seeing the
                // qualified/raw id -- matching apply_params_to_hardware_locked's own
                // actual forwarding-path input (see its call site above, and the
                // other to_j2534_config_id call site in this file).
                if param_id == ComParamId(j2534_0404::BS_TX)
                    && param_id.to_j2534_config_id(link.hw_protocol_id) == Some(j2534_0404::BS_TX)
                    && !is_valid_block_size_override(value)
                {
                    return Err(Status::invalid_argument(format!(
                        "CP_BlockSizeOverride value {value} is out of range (must be 0x00..=0xFF, or 0xFFFF to \
                         use the vehicle-reported value)"
                    )));
                }
                // CP_InitializationSettings: only 1 (5-baud init), 2 (fast
                // init), or 3 (no init) are meaningful (ADR-074); any other
                // value would otherwise silently fall back to the legacy
                // init-sequence heuristic at CoptStartcomm time.
                if param_id == PARAM_INIT_SETTINGS && !is_valid_init_settings(value) {
                    return Err(Status::invalid_argument(format!(
                        "CP_InitializationSettings value {value} is out of range \
                         (must be 1 [5-baud init], 2 [fast init], or 3 [no init])"
                    )));
                }
                // CP_5BaudAddressFunc / CP_5BaudAddressPhys (ADR-076): both are
                // single address bytes sent at 5 baud during the spec-mandated
                // 5-baud init wakeup sequence, so a value that does not fit in
                // one byte can never be forwarded to the adapter -- reject it
                // here rather than truncating it silently at CoptStartcomm
                // time (mirrors the CP_InitializationSettings check above).
                if (param_id == PARAM_5BAUD_ADDR_FUNC || param_id == PARAM_5BAUD_ADDR_PHYS)
                    && value > 0xFF
                {
                    return Err(Status::invalid_argument(format!(
                        "{} value {value} is out of range (must fit in a single address byte, 0..=0xFF)",
                        if param_id == PARAM_5BAUD_ADDR_FUNC {
                            "CP_5BaudAddressFunc"
                        } else {
                            "CP_5BaudAddressPhys"
                        }
                    )));
                }
                // CP_PhysReqFormatPriorityType / CP_PhysReqTargetAddr /
                // CP_FuncReqFormatPriorityType / CP_FuncReqTargetAddr /
                // CP_Node_Address (PR #53 review round, corrected in a later
                // round -- see `j2534-0404-service/docs/implementation-notes.md`):
                // all five are client-writable via SetComParam on both
                // J1850PWM/J1850VPW and ISO9141/ISO14230 (KWP) links
                // (previously stuck with whatever a resource preset's seeded
                // defaults supplied), and `tx_header::j1850_header_bytes` /
                // `tx_header::kwp_header_bytes` each narrow these with
                // `as u8` to build the outgoing addressing header -- a value
                // above 0xFF silently truncates on the wire (e.g. staging
                // 0x133 transmits 0x33) while GetComParam still reports back
                // the full out-of-range value. Reject here rather than
                // truncating silently, mirroring the CP_5BaudAddressFunc/
                // CP_5BaudAddressPhys check above. Scoped to J1850-family and
                // KWP-family links only: CAN also allows SetComParam of these
                // five IDs (`comparam_support::is_can_param`), but nothing on
                // the CAN forwarding path ever reads or truncates them, so a
                // CAN link must not reject an out-of-range value here.
                if let Some(name) = j1850_or_kwp_addressing_byte_param_name(param_id)
                    && (protocol.is_j1850_family() || protocol.is_kwp_family())
                    && value > 0xFF
                {
                    return Err(Status::invalid_argument(format!(
                        "{name} value {value} is out of range (must fit in a single address \
                         byte, 0..=0xFF)"
                    )));
                }
                // CP_TesterSourceAddress (Codex review, PR #72 round 8): the
                // J1850/KWP-scoped check above deliberately excludes J1939
                // (whose links did not allow SetComParam of `NODE_ADDRESS`
                // when that check was written), but ADR-179 Decision 3's own
                // client-readback fix later admitted `NODE_ADDRESS` into
                // `is_j1939_param`'s allowlist, and a J1939 link with address
                // negotiation disabled (`CP_J1939AddressNegotiationRule` bit
                // 1) relies on the client supplying it directly -- there is
                // no claim-loop writeback to overwrite an invalid value in
                // that case. `tx_header::j1939_header_bytes`'s caller
                // (`tester_addr`) narrows this with `as u8` the same way
                // `j1850_header_bytes`/`kwp_header_bytes` narrow their own
                // addressing bytes, so the same silent-truncation risk
                // applies. Reject here rather than truncating, using
                // ISO 22900-2's own `CP_TesterSourceAddress` name (not
                // `j1850_or_kwp_addressing_byte_param_name`'s `CP_Node_
                // Address`, which is that check's own K-line/J1850-flavored
                // alias for the same underlying `NODE_ADDRESS` ComParam).
                if param_id == ComParamId(j2534_0404::NODE_ADDRESS)
                    && protocol.is_j1939_family()
                    && value > 0xFF
                {
                    return Err(Status::invalid_argument(format!(
                        "CP_TesterSourceAddress value {value} is out of range (must fit in a \
                         single address byte, 0..=0xFF)"
                    )));
                }
                // CP_J1939PDUFormat / CP_J1939PDUSpecific (Codex review, PR
                // #72 round 8): both are one-byte wire fields
                // (`tx_header::j1939_header_bytes` narrows each with `as u8`
                // to compose the outgoing 29-bit CAN identifier's PF/PS
                // bytes), the same shape as `CP_J1939TargetAddress`'s own
                // round-6/7 fix -- an out-of-range value silently truncates
                // on the wire (e.g. staging PF `0x1EA` transmits `0xEA`,
                // potentially selecting a different PGN/addressing mode)
                // while `GetComParam` keeps reporting the untruncated value.
                // Reject here rather than truncating, mirroring the
                // `CP_TesterSourceAddress` check just above.
                if (param_id == PARAM_J1939_PDU_FORMAT || param_id == PARAM_J1939_PDU_SPECIFIC)
                    && value > 0xFF
                {
                    return Err(Status::invalid_argument(format!(
                        "{} value {value} is out of range (must fit in one byte, 0..=0xFF)",
                        if param_id == PARAM_J1939_PDU_FORMAT {
                            "CP_J1939PDUFormat"
                        } else {
                            "CP_J1939PDUSpecific"
                        }
                    )));
                }
                // CP_MessagePriority / CP_J1939DataPage (Codex review, PR #72
                // round 11): `tx_header::j1939_header_bytes` packs
                // `CP_MessagePriority` into byte 0 bits 4-2 (`priority &
                // 0x07`) and `CP_J1939DataPage`'s low bit into byte 0 bit 0
                // (`data_page & 0x01`) when composing the outgoing 29-bit CAN
                // identifier -- an out-of-range value (e.g. priority `8`,
                // data page `2`) silently masks to a different in-range value
                // on the wire while `GetComParam` keeps reporting the
                // untruncated value, changing both arbitration priority and
                // the selected PGN without surfacing an error. Reject here
                // rather than masking, mirroring the `CP_J1939PDUFormat`/
                // `CP_J1939PDUSpecific` check above. `CP_MessagePriority` is
                // gated on `protocol.is_j1939_family()` (mirroring the
                // `CP_TesterSourceAddress` check earlier in this function):
                // unlike `CP_J1939PDUFormat`/`_Specific`, it is allowlisted
                // on several unrelated protocol families too
                // (`comparam_support::is_can_param`/`is_kwp_param`/
                // `is_j1850pwm_param`/`is_j1850vpw_param`), none of whose own
                // composers read it via this J1939-specific 3-bit mask, so a
                // non-J1939 link must not reject a value this narrower J1939
                // field would otherwise consider out of range.
                // `CP_J1939DataPage` is J1939-only in `comparam_support`'s
                // own allowlists, so no family gate is needed for it.
                if param_id == PARAM_MESSAGE_PRIORITY && protocol.is_j1939_family() && value > 7 {
                    return Err(Status::invalid_argument(format!(
                        "CP_MessagePriority value {value} is out of range for a SAE J1939 link \
                         (must fit in the 3-bit priority field, 0..=7)"
                    )));
                }
                if param_id == PARAM_J1939_DATA_PAGE && value > 1 {
                    return Err(Status::invalid_argument(format!(
                        "CP_J1939DataPage value {value} is out of range (must be 0 or 1)"
                    )));
                }
                // CP_CANFDTxMaxDataLength (ADR-158): only the encodings SAE
                // J2534-2 Table 90/97 documents are meaningful -- everything
                // else is rejected here rather than silently accepted as an
                // unreachable FD-mode trigger value.
                if param_id == PARAM_CANFD_TX_MAX_DATA_LENGTH
                    && !is_valid_canfd_tx_max_data_length(value)
                {
                    return Err(Status::invalid_argument(format!(
                        "CP_CANFDTxMaxDataLength value {value} is out of range (accepted \
                         values: {CANFD_TX_MAX_DATA_LENGTH_ACCEPTED:?})"
                    )));
                }
                // CP_NdisPinOption (SAE J2534-2 clause 24 Ethernet_NDIS,
                // ADR-194/Phase 16): only `0` (auto), `1` (Option 1), or `2`
                // (Option 2) are meaningful -- `ndis_pin_option_connect_flags`
                // maps any other value to auto at `ConnectComLogicalLink`
                // time, which would otherwise let a client stage a bogus
                // value that silently resolves to the same connect flags as
                // an explicit `0`, with no error and `GetComParam` reporting
                // back the untranslated value. Reject here rather than
                // silently coercing, mirroring `CP_J1939DataPage`'s own
                // range-validation shape just above.
                if param_id == PARAM_NDIS_PIN_OPTION && value > 2 {
                    return Err(Status::invalid_argument(format!(
                        "CP_NdisPinOption value {value} is out of range (must be 0 [auto], 1 \
                         [Option 1], or 2 [Option 2])"
                    )));
                }
                // The ten CP_UebT*Min/Max UART Echo Byte timing ComParams
                // (SAE J2534-2 clause 12.3.4.1, ADR-216): SAE J2534-2's own
                // declared range for all ten is 0x0000..=0xFFFF -- reject
                // here rather than letting a batched SET_CONFIG silently fail
                // (dropping every OTHER staged ComParam in the same batch
                // along with it), mirroring CP_BlockSizeOverride's own
                // rejection shape above.
                if matches!(
                    param_id,
                    PARAM_UEB_T0_MIN
                        | PARAM_UEB_T1_MAX
                        | PARAM_UEB_T2_MAX
                        | PARAM_UEB_T3_MAX
                        | PARAM_UEB_T4_MIN
                        | PARAM_UEB_T5_MAX
                        | PARAM_UEB_T6_MAX
                        | PARAM_UEB_T7_MIN
                        | PARAM_UEB_T7_MAX
                        | PARAM_UEB_T9_MIN
                ) && value > 0xFFFF
                {
                    return Err(Status::invalid_argument(format!(
                        "UART Echo Byte timing ComParam {:#010x} value {value} is out of range \
                         (must fit in 0x0000..=0xFFFF)",
                        param_id.0
                    )));
                }
                // CP_AnalogReadingsPerMsg (SAE J2534-2 clause 10.3.3.2.4,
                // ADR-216): Table 16's own range is 1-1032 (0x408) --
                // clause 10.3.3.2.4's own zero-value special case (disarming
                // the acquisition subsystem entirely) is intentionally not
                // offered through this ComParam; CP_AnalogSampleRate remains
                // the sole arming/disarming knob (ADR-178's own established
                // contract for that ComParam).
                if param_id == PARAM_ANALOG_READINGS_PER_MSG && !(1..=0x408).contains(&value) {
                    return Err(Status::invalid_argument(format!(
                        "CP_AnalogReadingsPerMsg value {value} is out of range (must be \
                         1..=0x408 [1032])"
                    )));
                }
                // CP_AnalogSamplesPerReading (SAE J2534-2 clause 10.3.3.2.3,
                // ADR-216): Table 16's own range floor is 1 -- same
                // zero-value-is-CP_AnalogSampleRate's-job reasoning as
                // CP_AnalogReadingsPerMsg just above.
                if param_id == PARAM_ANALOG_SAMPLES_PER_READING && value == 0 {
                    return Err(Status::invalid_argument(
                        "CP_AnalogSamplesPerReading value 0 is out of range (must be >= 1; \
                         CP_AnalogSampleRate is the sole arming/disarming knob for the Analog \
                         Inputs acquisition subsystem)",
                    ));
                }
                // CP_AnalogSampleResolution / CP_AnalogInputRangeLow /
                // CP_AnalogInputRangeHigh (SAE J2534-2 clause 10.3.3.2.6-.2.8,
                // ADR-216): all three are genuinely read-only -- clause
                // 10.3.3.2.6-.2.8 each require the device itself to reject a
                // SET_CONFIG on any of these three with
                // `ERR_INVALID_IOCTL_PARAM_ID`, so rejecting synchronously in
                // this service, before ever reaching the native call, is a
                // direct application of the same constraint. Populated only
                // by the connect-time GET_CONFIG readback (ADR-216 Decision
                // item 6), never by SetComParam.
                if matches!(
                    param_id,
                    PARAM_ANALOG_SAMPLE_RESOLUTION
                        | PARAM_ANALOG_INPUT_RANGE_LOW
                        | PARAM_ANALOG_INPUT_RANGE_HIGH
                ) {
                    return Err(state_guard_status(
                        Code::InvalidArgument,
                        format!(
                            "ComParam {:#010x} is read-only (SAE J2534-2 clause 10.3.3.2.6-.2.8) \
                             -- SetComParam is not supported for it",
                            param_id.0
                        ),
                        PduError::PduErrComparamNotSupported,
                        link.last_error.clone(),
                    ));
                }
                // CP_TIdle (0x13) is stored under its own ID only -- it does
                // NOT also write W0/W5 here. The W0/W5 fan-out happens only
                // at the hardware-forwarding call sites, via `expand_tidle`
                // (ADR-072), which is order-independent: an explicit
                // SetComParam(W0/W5, ...) always wins over a TIDLE-derived
                // value regardless of whether it was set before or after
                // TIDLE. Mutating W0/W5's Working entries here (as an
                // earlier version of this code did) would make that
                // precedence depend on call order -- a later
                // SetComParam(TIDLE, ...) would silently overwrite an
                // earlier explicit SetComParam(W0/W5, ...) in the Working
                // set, which `expand_tidle` would then see as "explicit"
                // and forward incorrectly. See ADR-071 for the analogous
                // CP_UartConfig precedent: CP_UartConfig is stored under its
                // own ID only, and PARITY derivation happens at forwarding
                // time, not at SetComParam time.
                link.working.unum32.insert(param_id, value);
            }
            Some(vci_service_interface::param_item::ParamData::Bytefield(bytes)) => {
                let mut links = self.logical_links.lock().await;
                let link = links
                    .get_mut(&handle)
                    .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
                comparam_support::check_param_allowed(
                    ChannelProtocol::from_raw(link.base_hw_protocol_id()),
                    param_id,
                    link.last_error.clone(),
                )?;
                // CP_J1939SourceName is PDU_PC_UNIQUE_ID class (see
                // comparam_support::CAN_UNIQUE_ID_BYTES) and is already rejected by
                // check_param_allowed above (ADR-042); it does not belong here.
                //
                // Membership is checked via `comparam_support::is_bytefield_param`
                // (backed by `BYTEFIELD_PARAMS`) rather than a literal match here,
                // so this list can never drift from `rpc_get_com_param`'s
                // unseeded-Bytefield fallback (A2-18, ADR-133), which checks the
                // same const.
                if comparam_support::is_bytefield_param(param_id) {
                    // CP_TesterPresentMessage (ADR-215): ISO 22900-2's own
                    // MDF-style ComParam declaration caps this Bytefield's
                    // raw payload at ParamMaxLen = 12 bytes, independent of
                    // whatever composed on-wire length a given protocol
                    // ultimately allows (checked separately, at resolution
                    // time, by `resolve_tester_present`'s
                    // `sae_tx_size_range` call). Reject an over-length
                    // write here rather than storing it, mirroring the
                    // CP_UartConfig/CP_Parity/CP_BlockSizeOverride checks in
                    // the Unum32 arm above.
                    if param_id == PARAM_TESTER_PRESENT_MSG && bytes.len() > 12 {
                        return Err(Status::invalid_argument(format!(
                            "CP_TesterPresentMessage value is {} bytes, which exceeds the \
                             ISO 22900-2 ParamMaxLen of 12 bytes",
                            bytes.len()
                        )));
                    }
                    // A BUSTYPE_BYTES-class param (currently only
                    // CP_CanBaudrateRecord) with no default is exposed by
                    // `rpc_get_com_param` as an empty Bytefield, not an
                    // absent one (ADR-130). A client that round-trips
                    // that response straight back through SetComParam
                    // must not create a Working-vs-Active asymmetry that
                    // doesn't exist for any other unset BUSTYPE param.
                    // Normalize back to "absent" instead.
                    // Non-BUSTYPE bytefield params are unaffected: some
                    // presets above use an explicit empty Bytefield as a
                    // meaningful value (e.g. `CP_TesterPresentMessage`),
                    // so this normalization must stay scoped to
                    // `is_bustype_bytes_param`.
                    //
                    // ADR-133 amendment (Codex review round 2, PR #147):
                    // `bustype_params_differ` now compares *effective*
                    // values via `comparam_support::effective_bytes`, not
                    // raw map presence, so this normalization is no longer
                    // load-bearing for that differ specifically -- an
                    // explicit empty entry and an absent one now compare
                    // equal there either way. It remains load-bearing for
                    // `apply_bustype_lock`'s `hw_set` push path *only in the
                    // not-locked-by-another-CLL case*, where `hw_set` is an
                    // unfiltered clone of `params`: there, an explicit empty
                    // entry is still a distinct map key going into that
                    // computation, even though the differ treats it as
                    // equivalent to absent. When locked by another CLL,
                    // `hw_set` unconditionally strips every `BUSTYPE_BYTES`
                    // key regardless of this normalization, so it makes no
                    // difference to that path (`edge-case-hunter`, PR #147
                    // round 3 review).
                    if bytes.is_empty() && comparam_support::is_bustype_bytes_param(param_id) {
                        link.working.bytes.remove(&param_id);
                    } else {
                        link.working.bytes.insert(param_id, bytes);
                    }
                } else {
                    return Err(Status::unimplemented(format!(
                        "unsupported bytefield com_param_id {:#010x}",
                        param_id.0
                    )));
                }
            }
            Some(vci_service_interface::param_item::ParamData::Snum32(_)) => {
                // ADR-216 Decision item 8 (edge-case-hunter finding, round 2
                // correction): `CP_AnalogInputRangeLow`/`CP_AnalogInputRangeHigh`
                // are the only two ComParams this service ever reports via the
                // `Snum32` oneof arm (GetComParam), and both are genuinely
                // read-only -- a client that naturally round-trips a `GetComParam`
                // response straight back through `SetComParam` sends exactly this
                // arm, not `Unum32`, so the read-only rejection must be reachable
                // here too, not only from the `Unum32` arm above (which a client
                // would only hit by deliberately mismatching the oneof shape).
                // There is no writable Snum32-shaped ComParam in this codebase at
                // all, so every other param_id here falls through to the same
                // generic `unimplemented` the Bytefield/Structfield arms use for
                // an unrecognized member.
                let mut links = self.logical_links.lock().await;
                let link = links
                    .get_mut(&handle)
                    .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
                comparam_support::check_param_allowed(
                    ChannelProtocol::from_raw(link.base_hw_protocol_id()),
                    param_id,
                    link.last_error.clone(),
                )?;
                if matches!(
                    param_id,
                    PARAM_ANALOG_INPUT_RANGE_LOW | PARAM_ANALOG_INPUT_RANGE_HIGH
                ) {
                    return Err(state_guard_status(
                        Code::InvalidArgument,
                        format!(
                            "ComParam {:#010x} is read-only (SAE J2534-2 clause 10.3.3.2.7-.2.8) \
                             -- SetComParam is not supported for it",
                            param_id.0
                        ),
                        PduError::PduErrComparamNotSupported,
                        link.last_error.clone(),
                    ));
                }
                return Err(Status::unimplemented(format!(
                    "unsupported snum32 com_param_id {:#010x}",
                    param_id.0
                )));
            }
            Some(vci_service_interface::param_item::ParamData::Structfield(sf)) => {
                let mut links = self.logical_links.lock().await;
                let link = links
                    .get_mut(&handle)
                    .ok_or_else(|| unknown_handle_status(format!("unknown cll_handle {handle}")))?;
                comparam_support::check_param_allowed(
                    ChannelProtocol::from_raw(link.base_hw_protocol_id()),
                    param_id,
                    link.last_error.clone(),
                )?;
                // Membership via `comparam_support::is_structfield_param`
                // (backed by `STRUCTFIELD_PARAMS`), same rationale as the
                // Bytefield arm above.
                if comparam_support::is_structfield_param(param_id) {
                    // Codex review, PR #17: validate the oneof variant
                    // actually matches this param_id's expected shape, and
                    // (for AccessTiming) that every field fits the ISO
                    // 14230-2 wire-byte range, before ever storing it --
                    // otherwise a later ADR-146 Access Timing response can
                    // panic on a mismatched variant, or silently truncate an
                    // out-of-range value.
                    comparam_support::validate_structfield_shape(param_id, &sf)
                        .map_err(Status::invalid_argument)?;
                    link.working.structfield.insert(param_id, sf);
                } else {
                    return Err(Status::unimplemented(format!(
                        "unsupported structfield com_param_id {:#010x}",
                        param_id.0
                    )));
                }
            }
            _ => {
                return Err(Status::unimplemented(
                    "only unum32, bytefield, and structfield com parameters are supported by j2534-0404-service",
                ));
            }
        }

        Ok(Self::empty_response())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;

    /// Builds a minimal `J2534Service` backed by the mock J2534 cdylib, with
    /// `can_channel_mode = Auto` and no logical links registered -- for
    /// exercising `probe_can_channel_mode`'s own guards directly, without
    /// driving an actual RPC or the poll task. Mirrors
    /// `rpc_primitive.rs::tests::service_with_one_link`'s construction shape.
    async fn service_with_auto_can_mode_and_no_links() -> J2534Service {
        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

        J2534Service {
            api: Arc::new(Mutex::new(api)),
            startup_config: Arc::new(
                crate::config::parse_startup_arg("j2534-0404:mock-lib").unwrap(),
            ),
            can_channel_mode: CanChannelMode::Auto,
            resolved_can_channel_mode: Arc::new(Mutex::new(None)),
            modules: Arc::new(vec![crate::config::ModuleEntry {
                label: "j2534-0404".to_string(),
                pname: None,
            }]),
            device_id: Arc::new(Mutex::new(None)),
            vendor_ioctls: Arc::new(HashMap::new()),
            device_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            periodic_clear_epoch: Arc::new(portable_atomic::AtomicU64::new(0)),
            logical_links: Arc::new(Mutex::new(HashMap::new())),
            drain_watermarks: Arc::new(Mutex::new(HashMap::new())),
            shared_channels: Arc::new(Mutex::new(HashMap::new())),
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

    /// ADR-107 addendum (Codex review, PR #110): `probe_can_channel_mode`
    /// must not cache a false `SingleChannel` conclusion for a `handle`
    /// whose CLL doesn't exist in `logical_links` at all -- the scenario a
    /// concurrent `ModuleDisconnect` racing the probe's own
    /// `ensure_uudt_companion_channel` call would produce (that call's own
    /// `unknown cll_handle` error is exactly what an absent-from-the-map
    /// `handle` reproduces directly, without needing to actually time the
    /// race). Caching `SingleChannel` here would poison the cache for the
    /// NEXT module's real probe.
    #[tokio::test]
    async fn probe_can_channel_mode_does_not_cache_for_a_dead_handle() {
        let service = service_with_auto_can_mode_and_no_links().await;
        const DEAD_HANDLE: u32 = 999;
        assert!(
            !service
                .logical_links
                .lock()
                .await
                .contains_key(&DEAD_HANDLE),
            "sanity check: DEAD_HANDLE must not be a real CLL"
        );

        service.probe_can_channel_mode(DEAD_HANDLE).await;

        assert!(
            service.resolved_can_channel_mode.lock().await.is_none(),
            "a probe for a handle absent from logical_links must leave the cache unset, not \
             poison it with a false SingleChannel conclusion"
        );
    }

    /// ADR-107 addendum (edge-case-hunter follow-up, PR #110): the write-back
    /// guard is a two-conjunct AND (`slot.is_some() &&
    /// logical_links.contains_key(&handle)`); the test above only exercises
    /// the "both false" corner (no device open, dead handle). This covers
    /// the conjunct actually in question here -- a device IS open (as a
    /// concurrent `ModuleDisconnect` + reopen under a DIFFERENT device would
    /// leave it) while `handle`'s CLL is STILL absent from `logical_links` --
    /// proving `contains_key(&handle)` is independently load-bearing rather
    /// than redundant with `slot.is_some()`. Reachable deterministically
    /// here because `ensure_uudt_companion_channel`'s `unknown cll_handle`
    /// short-circuit (which also drives the dead-handle test above) returns
    /// before ever touching `device_id`, so a device opened directly in this
    /// test's setup is left untouched by the probe and is still open when
    /// the write-back guard runs.
    #[tokio::test]
    async fn probe_can_channel_mode_does_not_cache_for_a_dead_handle_with_a_device_open() {
        let service = service_with_auto_can_mode_and_no_links().await;
        const DEAD_HANDLE: u32 = 999;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        assert!(
            !service
                .logical_links
                .lock()
                .await
                .contains_key(&DEAD_HANDLE),
            "sanity check: DEAD_HANDLE must not be a real CLL"
        );

        service.probe_can_channel_mode(DEAD_HANDLE).await;

        assert!(
            service.resolved_can_channel_mode.lock().await.is_none(),
            "a probe for a handle absent from logical_links must leave the cache unset even \
             when a device IS open -- the write-back guard's contains_key(&handle) conjunct \
             must be load-bearing on its own, not merely redundant with slot.is_some()"
        );
        assert!(
            service.device_id.lock().await.is_some(),
            "sanity check: the device opened in this test's setup must still be open -- this \
             test is only meaningful if slot.is_some() actually held true at write-back time"
        );
    }

    /// ADR-126 round 2 (`edge-case-hunter` follow-up on the `connect_in_flight`
    /// fix): a plain, synchronous unit test for `pdu_connect_begun()`'s
    /// `connect_in_flight`-`Weak::upgrade()` branch -- the full concurrency
    /// race this token closes (a real second RPC landing mid-connect) can't
    /// be forced without a mock `PassThruConnect` hold hook this codebase
    /// doesn't have (see ADR-126's round-2 Consequences), but that gap
    /// doesn't excuse leaving the toggle logic itself untested: does
    /// `pdu_connect_begun()` correctly read a live `connect_in_flight` as
    /// "begun," and correctly revert to `false` once the owning `Arc` drops.
    /// No async runtime, mock cdylib, or RPC needed.
    #[test]
    fn pdu_connect_begun_reflects_the_connect_in_flight_token() {
        let mut link = minimal_not_connected_link();
        assert!(
            !link.pdu_connect_begun(),
            "a fresh LogicalLinkState (connected: false, no token claimed) must not report \
             PDUConnect as begun"
        );

        let token = Arc::new(());
        link.connect_in_flight = Arc::downgrade(&token);
        assert!(
            link.pdu_connect_begun(),
            "a live connect_in_flight Weak must report PDUConnect as begun, even though \
             `connected` itself is still false"
        );

        drop(token);
        assert!(
            !link.pdu_connect_begun(),
            "once the owning Arc drops, connect_in_flight.upgrade() must return None again, \
             so pdu_connect_begun() must revert to false with no explicit clear anywhere"
        );

        // The other disjunct: `connected: true` alone (no token at all) must
        // also report begun -- pdu_connect_begun() is an OR of both sources.
        link.connected = true;
        assert!(
            link.pdu_connect_begun(),
            "connected: true alone (independent of connect_in_flight) must report PDUConnect \
             as begun"
        );
    }

    /// A minimal, never-connected `LogicalLinkState` -- `connected: false`
    /// makes `ensure_uudt_companion_channel` return `Ok(())` trivially
    /// (its own `already_open || !connected` short-circuit), without
    /// touching `device_id` or opening any physical channel. Used to reach
    /// `probe_can_channel_mode`'s `DualChannel` conclusion deterministically,
    /// without needing a real device/channel or timing a race. Mirrors
    /// `rpc_primitive.rs::tests::service_with_one_link`'s `LogicalLinkState`
    /// literal.
    fn minimal_not_connected_link() -> LogicalLinkState {
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
            comm_started: false,
            raw_mode: false,
            checksum_mode: false,
            connect_generation: 0,
            stop_comm_pending: false,
            channel_key: None,
            pin_select: None,
            channel_index: None,
            base_hw_protocol_override: None,
            rx_buf: Arc::new(Mutex::new(CllEventQueue::default())),
            working: ComParamSet::default(),
            active: ComParamSet::default(),
            tester_present_state: TesterPresentState::None,
            tester_present_base_tx_flags: 0,
            open_tp_discards: Vec::new(),
            working_unique_resp_id_table: Vec::new(),
            active_unique_resp_id_table: Vec::new(),
            unique_resp_filter_ids: Vec::new(),
            cancelled_cops: std::collections::HashSet::new(),
            held_lock_mask: 0,
            last_error: None,
            tx_held: std::collections::VecDeque::new(),
            tx_suspended_by_ioctl: false,
            tx_suspended_by_lock: false,
            tx_suspended_by_error: false,
            error_clear_seq: 0,
            error_set_seq: 0,
            client_filters: std::collections::HashMap::new(),
            repeat_message_ids: Vec::new(),
            pending_client_filters: std::collections::HashMap::new(),
            registrants: Vec::new(),
            next_registrant_seq: 0,
            j1939_claimed_address: None,
            j1939_claim_cursor: 0,
            j1939_negotiation_posture: J1939NegotiationPosture::Undecided,
            tp20_connection: None,
            tp20_broadcast_periodic: None,
        }
    }

    /// ADR-107 addendum (h), Fix 3 (holistic redesign, PR #110): the
    /// DUAL-conclusion counterpart to
    /// `probe_can_channel_mode_does_not_cache_for_a_dead_handle` above --
    /// the write-back guard (`slot.is_some() && logical_links.contains_key`)
    /// must reject a `DualChannel` conclusion just as it rejects a
    /// `SingleChannel` one. A genuinely-dual-capable probe additionally
    /// requires `ensure_uudt_companion_channel` to have returned `Ok`, which
    /// itself needs `handle` present in `logical_links` -- so, unlike the
    /// `SingleChannel` case above, "`handle` absent from `logical_links`
    /// throughout the call" cannot produce a `Dual` conclusion at all (this
    /// crate's `current_thread` test runtime makes a genuine narrow
    /// mid-probe removal infeasible to time deterministically here, per the
    /// established precedent in `rpc_primitive.rs::tests`' module doc
    /// comment). Instead this drives the SAME write-back guard through a
    /// real, deterministic `Dual` conclusion (`minimal_not_connected_link`,
    /// whose `connected: false` resolves `Dual` without needing a device at
    /// all) with no device open -- exercising the `slot.is_some()` half of
    /// the guard, which a concurrent `ModuleDisconnect` closing the device
    /// mid-probe would also trip.
    #[tokio::test]
    async fn probe_can_channel_mode_does_not_cache_a_dual_conclusion_with_no_device_open() {
        let service = service_with_auto_can_mode_and_no_links().await;
        const HANDLE: u32 = 1;
        service
            .logical_links
            .lock()
            .await
            .insert(HANDLE, minimal_not_connected_link());
        assert!(
            service.device_id.lock().await.is_none(),
            "sanity check: no device should be open"
        );

        service.probe_can_channel_mode(HANDLE).await;

        assert!(
            service.resolved_can_channel_mode.lock().await.is_none(),
            "a Dual conclusion reached with no device open must not be cached -- the write-back \
             guard must reject it the same way it rejects a Single conclusion for a dead handle"
        );
    }

    /// ADR-107 addendum (h), Fix 3: an epoch-mismatched cache entry (the
    /// device it was probed under has since closed, possibly reopened for a
    /// different module) must not be returned as a cache hit -- it forces a
    /// fresh probe, and a successful fresh probe overwrites the stale entry
    /// with the CURRENT epoch rather than leaving the old one in place.
    #[tokio::test]
    async fn probe_can_channel_mode_reprobes_and_overwrites_an_epoch_mismatched_entry() {
        let service = service_with_auto_can_mode_and_no_links().await;
        const HANDLE: u32 = 1;
        service
            .logical_links
            .lock()
            .await
            .insert(HANDLE, minimal_not_connected_link());

        // A device is open under the CURRENT epoch...
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service
            .device_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let current_epoch = service
            .device_epoch
            .load(std::sync::atomic::Ordering::SeqCst);

        // ...but the cache holds a conclusion stamped with an OLDER epoch,
        // as if probed under a device that has since closed and reopened.
        *service.resolved_can_channel_mode.lock().await =
            Some((current_epoch - 1, CanChannelMode::SingleChannel));

        service.probe_can_channel_mode(HANDLE).await;

        assert_eq!(
            *service.resolved_can_channel_mode.lock().await,
            Some((current_epoch, CanChannelMode::DualChannel)),
            "an epoch-mismatched entry must not be served as a cache hit -- \
             probe_can_channel_mode must re-probe (this CLL's `connected: false` trivially \
             resolves Dual) and overwrite the cache with a freshly-stamped CURRENT-epoch entry, \
             not silently keep serving the stale SingleChannel conclusion from the old epoch"
        );
    }

    /// ADR-107 addendum (h), Fix 4: `effective_can_channel_mode` must ignore
    /// a stale-epoch cached entry (rather than blindly returning whatever
    /// mode happens to be cached) and fall back to the same conservative
    /// `SingleChannel` default as a cold cache.
    #[tokio::test]
    async fn effective_can_channel_mode_ignores_a_stale_epoch_entry() {
        let service = service_with_auto_can_mode_and_no_links().await;
        service
            .device_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let current_epoch = service
            .device_epoch
            .load(std::sync::atomic::Ordering::SeqCst);

        // Cached under an OLDER epoch than the current one.
        *service.resolved_can_channel_mode.lock().await =
            Some((current_epoch - 1, CanChannelMode::DualChannel));

        assert_eq!(
            service.effective_can_channel_mode().await,
            CanChannelMode::SingleChannel,
            "a stale-epoch cached mode must not be used -- effective_can_channel_mode must fall \
             back to SingleChannel exactly as it would for a cold (never-probed) cache"
        );
    }

    /// ADR-107 addendum (Codex-review P1 finding on PR #110): the
    /// non-selecting `ensure_open_device` must re-validate its `cll_handle`
    /// against `logical_links` -- atomically with acquiring `device_id` --
    /// before returning or opening anything, so a probe resuming after its
    /// CLL has already been torn down by a concurrent `ModuleDisconnect`
    /// (which clears `logical_links` while still holding `device_id`)
    /// deterministically bails instead of reopening a device. A handle
    /// absent from `logical_links` reproduces that "already torn down"
    /// state directly, without needing to actually time the race (mirrors
    /// `probe_can_channel_mode_does_not_cache_for_a_dead_handle` above).
    /// Exercises both branches `ensure_open_device_inner` can take: with
    /// nothing open yet (this test) and with a device already open under a
    /// DIFFERENT module (below).
    #[tokio::test]
    async fn ensure_open_device_rejects_a_dead_cll_handle_with_nothing_open() {
        let service = service_with_auto_can_mode_and_no_links().await;
        const DEAD_HANDLE: u32 = 999;
        assert!(
            !service
                .logical_links
                .lock()
                .await
                .contains_key(&DEAD_HANDLE),
            "sanity check: DEAD_HANDLE must not be a real CLL"
        );

        let status = service
            .ensure_open_device(DEAD_HANDLE)
            .await
            .expect_err("a dead cll_handle must be rejected, not silently open a device");

        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(status.message(), "unknown cll_handle 999");
        let detail = vci_service_interface::error_detail_from_status(&status).unwrap();
        assert_eq!(detail.pdu_error, PduError::PduErrInvalidHandle as i32);
        assert!(
            service.device_id.lock().await.is_none(),
            "no device should have been opened for a dead cll_handle"
        );
    }

    /// Same check, but covering `ensure_open_device_inner`'s OTHER branch:
    /// a device is already open (under some other module) when the dead
    /// probe resumes. Without the liveness check running before this
    /// branch too, a dead handle would incorrectly succeed by returning
    /// whatever device happens to already be open -- exactly the "probes
    /// module 2's device using a stale CLL's context" scenario the ADR-107
    /// addendum closes.
    #[tokio::test]
    async fn ensure_open_device_rejects_a_dead_cll_handle_with_a_device_already_open() {
        let service = service_with_auto_can_mode_and_no_links().await;
        const DEAD_HANDLE: u32 = 999;
        let already_open = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, already_open));

        let status = service
            .ensure_open_device(DEAD_HANDLE)
            .await
            .expect_err("a dead cll_handle must be rejected even when a device is already open");

        assert_eq!(status.code(), Code::NotFound);
        let detail = vci_service_interface::error_detail_from_status(&status).unwrap();
        assert_eq!(detail.pdu_error, PduError::PduErrInvalidHandle as i32);
        assert_eq!(
            service.device_id.lock().await.as_ref().map(|(h, _)| *h),
            Some(DEFAULT_MODULE_HANDLE),
            "the already-open device's slot must be left untouched by the rejected call"
        );
    }

    fn can_resource() -> vci_service_interface::create_com_logical_link_request::Resource {
        vci_service_interface::create_com_logical_link_request::Resource::RscData(
            vci_service_interface::ResourceData {
                dlc_pin_data: vec![],
                bus_type: None,
                protocol: Some(vci_service_interface::resource_data::Protocol::ProtocolId(
                    j2534_0404::CAN,
                )),
            },
        )
    }

    /// ADR-131 Amendment (Codex review, PR #143, round 3 on the same
    /// mechanism): `CreateComLogicalLink` used to never check
    /// `module_state.status` at all, so a module a hard error had marked
    /// `PduModstNotAvail` (device left open) still let a client create a
    /// fresh CLL -- the first step of a sequence that went on to bypass
    /// `ModuleConnect`'s own sticky-`NotAvail` rejection (ADR-131) via
    /// `spawn_new_shared_channel`'s now-removed reset-to-`Ready` side
    /// effect. ISO 22900-2 Table 9 (§9.4.9, `PDUCreateComLogicalLink`) lists
    /// `PDU_ERR_MODULE_NOT_CONNECTED` as a valid return.
    #[tokio::test]
    async fn create_com_logical_link_rejects_when_module_marked_not_avail() {
        let service = service_with_auto_can_mode_and_no_links().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        let status = service
            .rpc_create_com_logical_link(Request::new(
                vci_service_interface::CreateComLogicalLinkRequest {
                    module_handle: Some(vci_service_interface::ModuleHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                    }),
                    resource: Some(can_resource()),
                    cll_create_flag: None,
                },
            ))
            .await
            .expect_err(
                "CreateComLogicalLink on a module a hard error marked NotAvail must reject",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrModuleNotConnected as i32);
    }

    /// ADR-134 (supersedes the ADR-131-Amendment-era test of the same
    /// mechanism): connecting a CLL that was already created BEFORE the
    /// module went `NotAvail` (so `CreateComLogicalLink`'s own gate, tested
    /// above, never ran for it) must now be rejected by
    /// `ConnectComLogicalLink`'s own gate rather than silently succeeding --
    /// closing the residual ADR-131's Amendment left open. It also pins
    /// that `module_state` stays `NotAvail`: this call must not reach
    /// `spawn_new_shared_channel`'s (now-removed) reset-to-`Ready` side
    /// effect via the rejected path either.
    #[tokio::test]
    async fn connect_com_logical_link_rejects_when_module_marked_not_avail() {
        let service = service_with_auto_can_mode_and_no_links().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));

        let cll_handle = service
            .rpc_create_com_logical_link(Request::new(
                vci_service_interface::CreateComLogicalLinkRequest {
                    module_handle: Some(vci_service_interface::ModuleHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                    }),
                    resource: Some(can_resource()),
                    cll_create_flag: None,
                },
            ))
            .await
            .expect("CreateComLogicalLink should succeed while the module is Ready")
            .into_inner()
            .cll_handle
            .expect("cll_handle should be present")
            .cll_handle;

        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        let status = service
            .rpc_connect_com_logical_link(Request::new(
                vci_service_interface::ConnectComLogicalLinkRequest {
                    cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle,
                    }),
                },
            ))
            .await
            .expect_err(
                "ConnectComLogicalLink on a CLL whose module a hard error marked NotAvail \
                 must reject (ADR-134)",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrModuleNotConnected as i32);

        assert_eq!(
            service.module_state.lock().await.status,
            vci_service_interface::PduModuleStatus::PduModstNotAvail,
            "the rejected connect must not have touched module_state"
        );
    }

    /// ADR-134: `ensure_uudt_companion_channel` is the single choke point for
    /// every companion-channel open/join, reached not only from
    /// `ConnectComLogicalLink`'s own tail and `probe_can_channel_mode` but
    /// also from `promote_unique_resp_id_table` (`CoptUpdateparam` execution
    /// on an already-connected CLL, `events::handle_update_param`) -- a path
    /// with no other `module_state` gate anywhere in its call chain before
    /// this ADR. Exercises the gate directly on an already-connected CLL
    /// (mirroring the shape `promote_unique_resp_id_table` would drive it
    /// through) to pin that a module a hard error marked `NotAvail` rejects
    /// rather than silently opening a fresh physical channel.
    #[tokio::test]
    async fn ensure_uudt_companion_channel_rejects_when_module_marked_not_avail() {
        let service = service_with_auto_can_mode_and_no_links().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        const HANDLE: u32 = 1;
        let mut link = minimal_not_connected_link();
        link.connected = true;
        link.channel_key = Some((j2534_0404::CAN, 500_000, 0, 0));
        service.logical_links.lock().await.insert(HANDLE, link);

        let status = service
            .ensure_uudt_companion_channel(HANDLE)
            .await
            .expect_err(
                "ensure_uudt_companion_channel on a module a hard error marked NotAvail must \
                 reject, not silently open a fresh companion channel",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrModuleNotConnected as i32);

        assert!(
            service.shared_channels.lock().await.is_empty(),
            "no companion channel should have been opened for a NotAvail module"
        );
    }

    /// ADR-147 fail-open fix, connect-time reset: isolates
    /// `finalize_connected_link`'s own `tx_suspended_by_error = false` reset
    /// deterministically, with no timing dependency. Unlike the integration
    /// test `reconnect_does_not_inherit_prior_sessions_error_suspension`
    /// (`tests/grpc_mock/queue_error_suspend.rs`), which drives a full
    /// disconnect-then-reconnect and so cannot distinguish this reset's
    /// contribution from `cancel_link_cops`'s pre-existing disconnect-time
    /// clear (`cancel_held_tx_items`'s `reset_suspended` branch), this test
    /// calls `finalize_connected_link` directly on a link whose
    /// `tx_suspended_by_error` is seeded `true` and never goes through a
    /// disconnect at all -- so the disconnect-time clear never runs, and
    /// only `finalize_connected_link`'s own reset can be responsible for the
    /// flag reading `false` afterward. Mirrors
    /// `ensure_uudt_companion_channel_rejects_when_module_marked_not_avail`
    /// above: a `LogicalLinkState` inserted directly into `logical_links`,
    /// then a private service method called directly, no mock hardware
    /// needed.
    #[tokio::test]
    async fn finalize_connected_link_resets_prior_sessions_error_suspension() {
        let service = service_with_auto_can_mode_and_no_links().await;

        const HANDLE: u32 = 1;
        let mut link = minimal_not_connected_link();
        link.tx_suspended_by_error = true;
        service.logical_links.lock().await.insert(HANDLE, link);

        let channel_key: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);
        let mut chans: HashMap<ChannelKey, SharedChannel> = HashMap::new();
        service
            .finalize_connected_link(
                HANDLE,
                ChannelId(1),
                channel_key,
                false,
                ComParamSet::default(),
                HashMap::new(),
                &mut chans,
                None,
                false,
                (None, None, None),
            )
            .await
            .expect("finalize_connected_link should succeed for a still-live handle");

        assert!(
            !service.logical_links.lock().await[&HANDLE].tx_suspended_by_error,
            "finalize_connected_link must reset tx_suspended_by_error to false so a fresh \
             session never inherits a prior session's error suspension (ADR-147)"
        );
    }

    /// Codex review finding (PR #97, 6th round): `LogicalLinkState::tp20_
    /// connection`'s own doc comment already promised a reset "at the next
    /// `ConnectComLogicalLink`'s finalization" (the same reconnect-only
    /// reasoning ADR-180 Decision 17 uses for `j1939_claimed_address`), but
    /// nothing actually implemented it until this fix -- `link.tp20_connection
    /// = None;`, added to `finalize_connected_link` alongside the pre-existing
    /// SAE J1939 claim-state resets. Proven the same way, and for the same
    /// reason, as `finalize_connected_link_resets_prior_sessions_error_
    /// suspension` just above: the ONE disconnect shape that actually leaves
    /// `tp20_connection` stale (`events::handle_channel_hard_error`) also
    /// unconditionally marks the whole module `PduModstNotAvail`
    /// (`handle_channel_hard_error`'s own end-of-function `state.status =
    /// PduModstNotAvail`), and ADR-131/ADR-134 make that status sticky until
    /// an explicit `ModuleDisconnect` -- which this crate's own
    /// `rpc_module_disconnect` implements by unconditionally clearing the
    /// ENTIRE `logical_links` map, destroying the very `LogicalLinkState`
    /// entry whose staleness this fix is protecting (confirmed against
    /// `connect_com_logical_link_rejects_when_module_marked_not_avail` above:
    /// `ConnectComLogicalLink` itself rejects with `PDU_ERR_MODULE_NOT_
    /// CONNECTED` for the SAME still-offline `cll_handle` until that full
    /// recovery cycle runs). So a real hard-error-then-reconnect-the-same-
    /// handle round trip is not constructible through this crate's gRPC
    /// surface at all (see `tests/grpc_mock/tp20.rs`'s own `hard_error_then_
    /// reconnect_on_the_same_channel_is_reported_as_module_not_avail`, which
    /// pins that currently-correct rejection instead and points back here for
    /// this fix's actual proof) -- this test calls `finalize_connected_link`
    /// directly on a link whose `tp20_connection` is seeded `Some(Established)`
    /// and never goes through a disconnect (clean or hard) at all, so only
    /// `finalize_connected_link`'s own reset can be responsible for it reading
    /// `None` afterward.
    #[tokio::test]
    async fn finalize_connected_link_resets_a_stale_tp20_connection() {
        let service = service_with_auto_can_mode_and_no_links().await;

        const HANDLE: u32 = 1;
        let mut link = minimal_not_connected_link();
        link.tp20_connection = Some(Tp20Connection {
            requested_rx_id: 0x0321,
            established_tx_id: Some(0x1000_0321),
            phase: Tp20ConnectionPhase::Established,
            passive: false,
        });
        service.logical_links.lock().await.insert(HANDLE, link);

        let channel_key: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);
        let mut chans: HashMap<ChannelKey, SharedChannel> = HashMap::new();
        service
            .finalize_connected_link(
                HANDLE,
                ChannelId(1),
                channel_key,
                false,
                ComParamSet::default(),
                HashMap::new(),
                &mut chans,
                None,
                false,
                (None, None, None),
            )
            .await
            .expect("finalize_connected_link should succeed for a still-live handle");

        assert_eq!(
            service.logical_links.lock().await[&HANDLE].tp20_connection,
            None,
            "finalize_connected_link must reset tp20_connection to None so a fresh generation \
             never inherits a prior generation's stale Established phase/TX-ID (Codex review, \
             PR #97)"
        );
    }

    /// ADR-216 Decision items 6/10, amended by Codex review (PR #130,
    /// Finding 1): `finalize_connected_link`'s own `analog_connect_sync`
    /// parameter write, isolated the same way as the two
    /// `finalize_connected_link_resets_*` tests just above -- a true
    /// end-to-end race (a client pipelining `ConnectComLogicalLink`
    /// immediately followed by `StartComPrimitive(CoptUpdateparam)` on the
    /// same handle, ahead of the connect response) is not practically
    /// constructible through this crate's gRPC test harness: `tonic`'s test
    /// client has no way to fire two calls on the same handle without
    /// waiting for the first to complete, so there is no way to land a
    /// second call inside the exact window between `finalize_connected_
    /// link`'s critical section starting and it publishing `connected`. This
    /// unit-level test of the new parameter's own handling is the
    /// substitute the task's verification instructions accept for that case
    /// -- it directly proves the shape the fix depends on (the write landing
    /// INSIDE this function's own critical section, ADR-216's actual fix)
    /// rather than the race itself, which is a property of this function
    /// being an atomic unit no other test can now observe torn.
    ///
    /// For `is_new_channel == true`, this also proves ordering against
    /// `link.active = working_snapshot`: `working_snapshot` seeds
    /// `CP_AnalogActiveChannels` to a stale value, and `analog_connect_sync`
    /// carries a different one -- the sync value must win, proving the write
    /// runs AFTER the `is_new_channel` promotion, not before (where it would
    /// be wholesale clobbered).
    #[tokio::test]
    async fn finalize_connected_link_writes_analog_connect_sync_for_a_new_channel() {
        let service = service_with_auto_can_mode_and_no_links().await;

        const HANDLE: u32 = 1;
        let link = minimal_not_connected_link();
        service.logical_links.lock().await.insert(HANDLE, link);

        let channel_key: ChannelKey = (j2534_0404::PROTOCOL_ANALOG_IN_1, 0, 0, 0);
        let mut chans: HashMap<ChannelKey, SharedChannel> = HashMap::new();

        let mut working_snapshot = ComParamSet::default();
        // A stale value the fresh-connect promotion (`link.active =
        // working_snapshot`) would otherwise leave in place -- the
        // analog_connect_sync write below must overwrite it.
        working_snapshot
            .unum32
            .insert(PARAM_ANALOG_ACTIVE_CHANNELS, 0xDEAD);

        let analog_connect_sync: AnalogConnectSync = (
            Some([
                (PARAM_ANALOG_ACTIVE_CHANNELS, 0x0003),
                (PARAM_ANALOG_SAMPLE_RESOLUTION, 12),
                (PARAM_ANALOG_INPUT_RANGE_LOW, 0xFFFF_B1E0),
                (PARAM_ANALOG_INPUT_RANGE_HIGH, 0x0000_1E00),
                (PARAM_ANALOG_AVERAGING_METHOD, 2),
            ]),
            Some(4),
            Some(8),
        );

        service
            .finalize_connected_link(
                HANDLE,
                ChannelId(1),
                channel_key,
                true,
                working_snapshot,
                HashMap::new(),
                &mut chans,
                None,
                false,
                analog_connect_sync,
            )
            .await
            .expect("finalize_connected_link should succeed for a still-live handle");

        let links = service.logical_links.lock().await;
        let link = &links[&HANDLE];
        for (param_id, expected) in [
            (PARAM_ANALOG_ACTIVE_CHANNELS, 0x0003),
            (PARAM_ANALOG_SAMPLE_RESOLUTION, 12),
            (PARAM_ANALOG_INPUT_RANGE_LOW, 0xFFFF_B1E0),
            (PARAM_ANALOG_INPUT_RANGE_HIGH, 0x0000_1E00),
            (PARAM_ANALOG_AVERAGING_METHOD, 2),
            (PARAM_ANALOG_SAMPLES_PER_READING, 4),
            (PARAM_ANALOG_READINGS_PER_MSG, 8),
        ] {
            assert_eq!(
                link.working.unum32.get(&param_id),
                Some(&expected),
                "Working must carry the analog_connect_sync value for {param_id:?}"
            );
            assert_eq!(
                link.active.unum32.get(&param_id),
                Some(&expected),
                "Active must carry the analog_connect_sync value for {param_id:?} -- and for \
                 CP_AnalogActiveChannels specifically, this must win over the stale \
                 working_snapshot value the is_new_channel promotion just set, proving the \
                 write runs after that promotion"
            );
        }
    }

    /// ADR-216 Decision items 6/10 amendment (Codex review, PR #130, Finding
    /// 1): the join case -- `analog_connect_sync` must write into Active
    /// even though `is_new_channel == false` leaves every other ComParam's
    /// Active at its default (the "Active stays default until CoptUpdateparam"
    /// rule documented just below the write in `finalize_connected_link`).
    #[tokio::test]
    async fn finalize_connected_link_writes_analog_connect_sync_for_a_joining_cll() {
        let service = service_with_auto_can_mode_and_no_links().await;

        const HANDLE: u32 = 1;
        let link = minimal_not_connected_link();
        service.logical_links.lock().await.insert(HANDLE, link);

        let channel_key: ChannelKey = (j2534_0404::PROTOCOL_ANALOG_IN_1, 0, 0, 0);
        let mut chans: HashMap<ChannelKey, SharedChannel> = HashMap::new();

        let analog_connect_sync: AnalogConnectSync = (
            Some([
                (PARAM_ANALOG_ACTIVE_CHANNELS, 0x0001),
                (PARAM_ANALOG_SAMPLE_RESOLUTION, 8),
                (PARAM_ANALOG_INPUT_RANGE_LOW, 0),
                (PARAM_ANALOG_INPUT_RANGE_HIGH, 0),
                (PARAM_ANALOG_AVERAGING_METHOD, 3),
            ]),
            Some(2),
            Some(5),
        );

        service
            .finalize_connected_link(
                HANDLE,
                ChannelId(1),
                channel_key,
                false,
                ComParamSet::default(),
                HashMap::new(),
                &mut chans,
                None,
                false,
                analog_connect_sync,
            )
            .await
            .expect("finalize_connected_link should succeed for a still-live handle");

        let links = service.logical_links.lock().await;
        let link = &links[&HANDLE];
        assert_eq!(
            link.active.unum32.get(&PARAM_ANALOG_AVERAGING_METHOD),
            Some(&3),
            "a joining CLL must still get the analog_connect_sync write into Active, unlike \
             every other ComParam (which stays default until CoptUpdateparam)"
        );
        assert_eq!(
            link.active.unum32.get(&PARAM_ANALOG_SAMPLES_PER_READING),
            Some(&2)
        );
        assert_eq!(
            link.active.unum32.get(&PARAM_ANALOG_READINGS_PER_MSG),
            Some(&5)
        );
    }

    /// ADR-134 Correction (Codex review, PR #149): `probe_sae_j1850_flavor`'s
    /// `active_probe` branch (the `ISO_15031_5_ON_SAE_J1850` OBD-active
    /// resource) transmits a real OBD Mode 01 PID 00 request on the vehicle
    /// bus, so ADR-134's original characterization of this pre-gate-2 window
    /// as harmless/read-only was wrong. This pins the fix: on a module a
    /// hard error marked `NotAvail`, the probe must short-circuit to
    /// `Fallback(J1850VPW)` -- the same outcome the pre-existing
    /// "could not open device at all" branch a few lines above already
    /// returns -- without ever calling `PassThruConnect`/`PassThruWriteMsgs`
    /// for either VPW/PWM candidate.
    ///
    /// `HANDLE` MUST be registered live in `logical_links` before calling
    /// (Codex review, PR #149, round 2): `ensure_open_device`'s
    /// `cll_liveness` check runs before this function's own module_state
    /// check and returns the identical `Fallback(J1850VPW)` value for an
    /// unregistered handle, so without a live entry this test would pass
    /// even with the NotAvail gate deleted entirely -- it would just be
    /// silently exercising the unrelated "unknown handle" branch instead.
    ///
    /// Known remaining test-coverage limitation, not a design residual
    /// (recorded in the Prioritized Backlog): even
    /// with `HANDLE` live, this assertion alone cannot distinguish "the gate
    /// short-circuited before probing" from "the gate was removed, the probe
    /// ran for real, and the mock's default silent bus made both VPW/PWM
    /// candidates naturally inconclusive" -- both converge on the identical
    /// `Fallback(J1850VPW)` return value. Proving the stronger claim (zero
    /// native `PassThruConnect`/`PassThruWriteMsgs` calls occurred) needs a
    /// `MockBackdoor`-style raw FFI dlopen of the same library path this
    /// module doesn't have (`j2534-0404-mock`'s plain Rust counters read the
    /// `rlib`'s own separate copy of the process-global mock state, not the
    /// dynamically-loaded `cdylib` instance `J2534Api0404::from_path`
    /// actually calls into -- see `tests/grpc_mock/harness.rs::MockBackdoor`'s
    /// doc comment, which exists specifically to work around that same
    /// split). The fix itself was verified correct by direct code
    /// inspection (the new gate is a plain early return, structurally
    /// identical to the pre-existing "could not open device" branch this
    /// same function already has).
    #[tokio::test]
    async fn probe_sae_j1850_flavor_skips_the_active_probe_when_module_marked_not_avail() {
        let service = service_with_auto_can_mode_and_no_links().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));
        service.module_state.lock().await.status =
            vci_service_interface::PduModuleStatus::PduModstNotAvail;

        // Codex review, PR #149: `HANDLE` must be a live CLL in
        // `logical_links`, or `ensure_open_device`'s `cll_liveness` check
        // (`ensure_open_device_inner`) rejects it as an unknown handle before
        // this function's own module_state check ever runs -- that
        // "could not open device" branch returns the identical
        // `Fallback(J1850VPW)` value, so without a registered handle this
        // test would pass even with the new NotAvail gate deleted entirely.
        const HANDLE: u32 = 1;
        service
            .logical_links
            .lock()
            .await
            .insert(HANDLE, minimal_not_connected_link());

        let outcome = service
            .probe_sae_j1850_flavor(HANDLE, ChannelProtocol::ISO_15031_5_ON_SAE_J1850, 100, None)
            .await;

        assert_eq!(
            outcome,
            J1850ProbeOutcome::Fallback(j2534_0404::J1850VPW),
            "a NotAvail module must short-circuit to the VPW fallback, matching the \
             could-not-open-device branch, rather than transmitting the active OBD probe"
        );
    }

    /// Builds a `SharedChannel` for direct insertion into `shared_channels`
    /// in a test -- mirrors `spawn_new_shared_channel`'s own construction
    /// shape, with a caller-supplied `dead` flag (ADR-134 Correction, Codex
    /// review round 6, PR #149).
    fn dead_shared_channel_for_test(dead: bool) -> SharedChannel {
        SharedChannel {
            channel_id: ChannelId(9999),
            ref_count: 1,
            tx_queue: tokio::sync::mpsc::unbounded_channel().0,
            executing_cop: Arc::new(Mutex::new(None)),
            _poll_cancel: tokio::sync::oneshot::channel().0,
            connect_flags: 0,
            dead,
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

    /// ADR-134 Correction (Codex review round 6, PR #149): a Codex finding
    /// showed the original three ADR-134 gates don't close the race between
    /// `events::handle_channel_hard_error` releasing `shared_channels` (its
    /// per-CLL teardown) and setting `module_state` to `NotAvail` a bit
    /// later -- a concurrent `ConnectComLogicalLink` reading `module_state`
    /// in that exact gap would still see `Ready` and could join the
    /// just-hard-errored `SharedChannel`, whose poll task is exiting, as if
    /// it were healthy. `handle_channel_hard_error` now marks that entry
    /// `dead` under the same `shared_channels` guard as its CLL teardown
    /// (closing before `module_state` is ever touched), and both
    /// existing-channel-join branches reject a `dead` entry. This test pins
    /// the rejection directly: `module_state` is deliberately left `Ready`
    /// (simulating exactly the racy read the finding described) so the
    /// assertion demonstrates the `dead` check itself is what rejects the
    /// join, not the pre-existing `module_state.status` gate.
    #[tokio::test]
    async fn connect_com_logical_link_rejects_a_dead_shared_channel_even_when_module_is_ready() {
        let service = service_with_auto_can_mode_and_no_links().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));

        let cll_handle = service
            .rpc_create_com_logical_link(Request::new(
                vci_service_interface::CreateComLogicalLinkRequest {
                    module_handle: Some(vci_service_interface::ModuleHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                    }),
                    resource: Some(can_resource()),
                    cll_create_flag: None,
                },
            ))
            .await
            .expect("CreateComLogicalLink should succeed while the module is Ready")
            .into_inner()
            .cll_handle
            .expect("cll_handle should be present")
            .cll_handle;

        let channel_key: ChannelKey = {
            let links = service.logical_links.lock().await;
            let link = links.get(&cll_handle).expect("cll should be registered");
            (
                link.hw_protocol_id,
                link.working.baud_rate(),
                link.pin_select.unwrap_or(0),
                0,
            )
        };
        service
            .shared_channels
            .lock()
            .await
            .insert(channel_key, dead_shared_channel_for_test(true));

        let status = service
            .rpc_connect_com_logical_link(Request::new(
                vci_service_interface::ConnectComLogicalLinkRequest {
                    cll_handle: Some(vci_service_interface::ComLogicalLinkHandle {
                        module_handle: DEFAULT_MODULE_HANDLE,
                        cll_handle,
                    }),
                },
            ))
            .await
            .expect_err(
                "ConnectComLogicalLink must reject joining a channel a hard error marked dead, \
                 even while module_state still reads Ready",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrModuleNotConnected as i32);

        assert_eq!(
            service.module_state.lock().await.status,
            vci_service_interface::PduModuleStatus::PduModstReady,
            "module_state must remain untouched by this rejection -- it is the dead-channel \
             check firing, not the pre-existing module_state.status gate"
        );
    }

    /// The `ensure_uudt_companion_channel` counterpart to the test above --
    /// same scenario, same reasoning, the sibling join branch this ADR's
    /// Correction also gates.
    #[tokio::test]
    async fn ensure_uudt_companion_channel_rejects_a_dead_shared_channel_even_when_module_is_ready()
    {
        let service = service_with_auto_can_mode_and_no_links().await;
        let device_id = service.api.lock().await.open(None).expect("mock open");
        *service.device_id.lock().await = Some((DEFAULT_MODULE_HANDLE, device_id));

        const HANDLE: u32 = 1;
        let mut link = minimal_not_connected_link();
        link.connected = true;
        link.channel_key = Some((j2534_0404::CAN, 500_000, 0, 0));
        service.logical_links.lock().await.insert(HANDLE, link);

        service.shared_channels.lock().await.insert(
            (j2534_0404::CAN, 500_000, 0, 0),
            dead_shared_channel_for_test(true),
        );

        let status = service
            .ensure_uudt_companion_channel(HANDLE)
            .await
            .expect_err(
                "ensure_uudt_companion_channel must reject joining a channel a hard error \
                 marked dead, even while module_state still reads Ready",
            );
        assert_eq!(status.code(), Code::FailedPrecondition);
        let detail = vci_service_interface::error_detail_from_status(&status)
            .expect("rejection should carry an ErrorDetail");
        assert_eq!(detail.pdu_error, PduError::PduErrModuleNotConnected as i32);

        assert_eq!(
            service.module_state.lock().await.status,
            vci_service_interface::PduModuleStatus::PduModstReady,
            "module_state must remain untouched by this rejection -- it is the dead-channel \
             check firing, not the pre-existing module_state.status gate"
        );
    }

    /// `edge-case-hunter` verification finding on the ADR-161 dedicated follow-up (see that
    /// ADR's Correction note): the fix's single most safety-critical property is that
    /// `ensure_uudt_companion_channel`'s "CLL destroyed concurrently" rollback branch calls
    /// `rollback_channel_join` -- which reuses the caller's own already-held `chans` guard --
    /// rather than the self-relocking `release_shared_channel_ref`, which would self-deadlock
    /// there now that `chans` stays held through that branch (`tokio::sync::Mutex` is
    /// non-reentrant). A regression back to the self-relocking call would hang rather than fail
    /// a test outright, since `#[tokio::test]` has no default timeout.
    ///
    /// Reproducing the actual concurrent-destroy race deterministically isn't feasible with this
    /// crate's single-threaded `current_thread` test runtime -- there is no `.await` yield point
    /// inside `ensure_uudt_companion_channel`'s own `shared_channels`-held span for a test to
    /// land a concurrent `DestroyComLogicalLink` in, the same infeasibility class as this file's
    /// other documented races. This instead directly exercises `rollback_channel_join`'s
    /// already-held-guard invocation shape in isolation, mirroring
    /// `restore_occupancy_epoch_on_rollback`'s own direct-unit-test precedent
    /// (`rpc_misc.rs::rollback_does_not_erase_a_surviving_siblings_newer_epoch`): calling it
    /// while the caller's own `chans` guard is still held must complete without hanging and
    /// correctly decrement/remove the entry, proving the shape itself works, not just that it
    /// type-checks.
    #[tokio::test]
    async fn rollback_channel_join_completes_under_the_callers_own_already_held_guard() {
        let service = service_with_auto_can_mode_and_no_links().await;
        let channel_key = (j2534_0404::CAN, 500_000, 0, 0);

        let mut chans = service.shared_channels.lock().await;
        chans.insert(channel_key, dead_shared_channel_for_test(false));

        // Reusing the caller's own already-held `chans` guard: if this internally tried to
        // re-lock `shared_channels` (the regression this test guards against), it would hang
        // here rather than return, since `tokio::sync::Mutex` is non-reentrant.
        service
            .rollback_channel_join(&mut chans, channel_key, None)
            .await;

        assert!(
            chans.get(&channel_key).is_none(),
            "a single-occupant channel's rollback must decrement ref_count to 0 and remove the \
             entry"
        );
    }

    /// ADR-158 correction (Codex review PR #30 round 4): locks in
    /// `fd_can_tx_message_size_range`'s three cases -- unset/Classic-max
    /// `TX_DL` falls back to `8` (not `0`, which would reject every
    /// payload), and a staged `TX_DL` widens the range to match it exactly.
    #[test]
    fn fd_can_tx_message_size_range_tracks_staged_tx_dl_with_an_eight_byte_floor() {
        let unset = ComParamSet::default();
        assert_eq!(fd_can_tx_message_size_range(&unset), 4..=12);

        let mut classic_max = ComParamSet::default();
        classic_max.unum32.insert(PARAM_CANFD_TX_MAX_DATA_LENGTH, 8);
        assert_eq!(fd_can_tx_message_size_range(&classic_max), 4..=12);

        let mut staged_32 = ComParamSet::default();
        staged_32.unum32.insert(PARAM_CANFD_TX_MAX_DATA_LENGTH, 32);
        assert_eq!(fd_can_tx_message_size_range(&staged_32), 4..=36);

        let mut staged_64 = ComParamSet::default();
        staged_64.unum32.insert(PARAM_CANFD_TX_MAX_DATA_LENGTH, 64);
        assert_eq!(fd_can_tx_message_size_range(&staged_64), 4..=68);
    }

    /// Builds a `UniqueRespIdTable` entry from `PDU_PC_UNIQUE_ID`-class
    /// Unum32 params -- mirrors `tx_header.rs::tests::entry_with`'s identical
    /// per-file-duplicated shape (this codebase's existing convention).
    fn urid_entry(params: &[(ComParamId, u32)]) -> EcuUniqueRespEntry {
        let mut set = ComParamSet::default();
        for &(id, value) in params {
            set.unum32.insert(id, value);
        }
        EcuUniqueRespEntry {
            unique_resp_identifier: 1,
            params: set,
        }
    }

    /// Backlog fix (round 19 follow-up, Codex review PR #42): `can_connect_
    /// flags`'s USDT observation used a raw `contains_key` check instead of
    /// [`usdt_resp_id`]-style filtering, unlike its own UUDT branch a few
    /// lines below (which already filtered correctly) -- so a
    /// sentinel-valued `CP_CanRespUSDTId` (`0xFFFFFFFF`, ISO 22900-2 Table
    /// 76's "not used" marker) still contributed its `CP_CanRespUSDTFormat`
    /// bits toward the connect-time `CAN_29BIT_ID`/`CAN_ID_BOTH` flag
    /// computation. An entry with ONLY a sentinel USDT id (29-bit format,
    /// no `CP_CanPhysReqId`, no UUDT id) and an empty link-level `working`
    /// set must now resolve to no flags at all -- before this fix, the
    /// sentinel wrongly counted as a real 29-bit observation and returned
    /// `CAN_29BIT_ID`.
    #[test]
    fn can_connect_flags_ignores_sentinel_usdt_id() {
        let entry = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0xFFFF_FFFF),
            (PARAM_CAN_RESP_USDT_FORMAT, 0x02), // bit 1: 29-bit CAN Id
        ]);
        let working = ComParamSet::default();
        assert_eq!(
            can_connect_flags(&working, &[entry]),
            0,
            "a sentinel-valued CP_CanRespUSDTId must not contribute an observation"
        );
    }

    /// ADR-162 Decision 2: an 11-bit USDT match key and a 29-bit UUDT match
    /// key at the same numeric id must NOT collide -- the match key is id +
    /// width + extended-addressing byte, not the bare numeric id (a bare-id
    /// comparison would over-reject this case).
    #[test]
    fn find_native_mixed_uudt_usdt_collision_distinguishes_11_bit_from_29_bit_same_numeric_id() {
        let entry = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x100),
            (PARAM_CAN_PHYS_REQ_ID, 0x200),
            (PARAM_CAN_RESP_UUDT_ID, 0x100),
            (PARAM_CAN_RESP_UUDT_FORMAT, 0x02), // bit 1: 29-bit CAN Id
        ]);
        assert!(
            find_native_mixed_uudt_usdt_collision(&[&[entry]]).is_none(),
            "an 11-bit USDT match key and a 29-bit UUDT match key at the same numeric id must \
             not collide"
        );
    }

    /// edge-case-hunter coverage gap (PR #35, round 3 verification): a
    /// different numeric id must never overlap, even when width and
    /// `ext_addr` both match -- `can_filter_keys_overlap`'s id check is the
    /// very first short-circuit, but no prior test isolated it from the
    /// width/ext_addr checks that follow it.
    #[test]
    fn find_native_mixed_uudt_usdt_collision_requires_the_same_numeric_id() {
        const EXTENDED_FC_ENABLED: u32 = 0x08 | 0x01;

        let entry = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x100),
            (PARAM_CAN_RESP_USDT_FORMAT, EXTENDED_FC_ENABLED),
            (PARAM_CAN_RESP_USDT_EXT_ADDR, 0xAA),
            (PARAM_CAN_PHYS_REQ_ID, 0x200),
            (PARAM_CAN_RESP_UUDT_ID, 0x101),
            (PARAM_CAN_RESP_UUDT_FORMAT, EXTENDED_FC_ENABLED),
            (PARAM_CAN_RESP_UUDT_EXT_ADDR, 0xAA),
        ]);
        assert!(
            find_native_mixed_uudt_usdt_collision(&[&[entry]]).is_none(),
            "a different numeric id must not collide even when width and ext_addr both match"
        );
    }

    /// ADR-162 Decision 2: the extended-addressing byte is part of the match
    /// key -- same id and width but a different extension byte must not
    /// collide, while an identical extension byte must.
    #[test]
    fn find_native_mixed_uudt_usdt_collision_compares_the_extended_addressing_byte() {
        // bit 3 (extended addressing) | bit 0 (flow control enabled) --
        // flow control must stay enabled here so the USDT key remains
        // collision-eligible; this test is about the extended-addressing
        // byte specifically, not the flow-control exclusion (covered
        // separately above).
        const EXTENDED_FC_ENABLED: u32 = 0x08 | 0x01;

        let different_ext_addr = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x100),
            (PARAM_CAN_RESP_USDT_FORMAT, EXTENDED_FC_ENABLED),
            (PARAM_CAN_RESP_USDT_EXT_ADDR, 0xAA),
            (PARAM_CAN_PHYS_REQ_ID, 0x200),
            (PARAM_CAN_RESP_UUDT_ID, 0x100),
            (PARAM_CAN_RESP_UUDT_FORMAT, EXTENDED_FC_ENABLED),
            (PARAM_CAN_RESP_UUDT_EXT_ADDR, 0xBB),
        ]);
        assert!(
            find_native_mixed_uudt_usdt_collision(&[&[different_ext_addr]]).is_none(),
            "same id and width but a different extended-addressing byte must not collide"
        );

        let same_ext_addr = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x100),
            (PARAM_CAN_RESP_USDT_FORMAT, EXTENDED_FC_ENABLED),
            (PARAM_CAN_RESP_USDT_EXT_ADDR, 0xAA),
            (PARAM_CAN_PHYS_REQ_ID, 0x200),
            (PARAM_CAN_RESP_UUDT_ID, 0x100),
            (PARAM_CAN_RESP_UUDT_FORMAT, EXTENDED_FC_ENABLED),
            (PARAM_CAN_RESP_UUDT_EXT_ADDR, 0xAA),
        ]);
        assert!(
            find_native_mixed_uudt_usdt_collision(&[&[same_ext_addr]]).is_some(),
            "identical id, width, AND extended-addressing byte must collide"
        );
    }

    /// Codex review, PR #35: under NORMAL (non-extended) addressing,
    /// `ext_addr` is never appended to the actual native filter data
    /// (`can_filter_message`), so it must be ignored by the match-key
    /// comparison too -- a differing `ext_addr` under normal addressing must
    /// still collide, unlike the extended-addressing case above where it is
    /// part of the real wire format and genuinely distinguishes two
    /// addresses.
    #[test]
    fn find_native_mixed_uudt_usdt_collision_ignores_ext_addr_under_normal_addressing() {
        let entry = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x100),
            (PARAM_CAN_RESP_USDT_FORMAT, 0x01), // flow control enabled, normal addressing
            (PARAM_CAN_RESP_USDT_EXT_ADDR, 0xAA),
            (PARAM_CAN_PHYS_REQ_ID, 0x200),
            (PARAM_CAN_RESP_UUDT_ID, 0x100),
            (PARAM_CAN_RESP_UUDT_FORMAT, 0x00), // normal addressing
            (PARAM_CAN_RESP_UUDT_EXT_ADDR, 0xBB),
        ]);
        let collision = find_native_mixed_uudt_usdt_collision(&[&[entry]]).expect(
            "same id/width under normal addressing must collide regardless of ext_addr, since \
             the byte is never part of the actual hardware filter message in that mode",
        );
        assert_eq!(collision.id, 0x100);
    }

    /// Codex review, PR #35: a NORMAL-addressed key's native filter is a
    /// strict superset of any EXTENDED-addressed key's filter at the same
    /// id/width -- `can_filter_message` builds the normal-addressed filter
    /// as a bare 4-byte id match (any data content), so it also matches
    /// every frame the extended-addressed filter's narrower 5-byte
    /// (id + ext_addr) pattern would match. Requiring `extended_addressing`
    /// to be equal (the pre-fix behavior) missed this real overlap entirely.
    /// Checked in both directions: USDT normal / UUDT extended, and USDT
    /// extended / UUDT normal.
    #[test]
    fn find_native_mixed_uudt_usdt_collision_detects_normal_vs_extended_addressing_overlap() {
        let usdt_normal_uudt_extended = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x100),
            (PARAM_CAN_RESP_USDT_FORMAT, 0x01), // normal addressing, flow control enabled
            (PARAM_CAN_PHYS_REQ_ID, 0x200),
            (PARAM_CAN_RESP_UUDT_ID, 0x100),
            (PARAM_CAN_RESP_UUDT_FORMAT, 0x08), // extended addressing
            (PARAM_CAN_RESP_UUDT_EXT_ADDR, 0xAA),
        ]);
        assert!(
            find_native_mixed_uudt_usdt_collision(&[&[usdt_normal_uudt_extended]]).is_some(),
            "a normal-addressed USDT filter matches any data content, so it overlaps an \
             extended-addressed UUDT id at the same numeric id regardless of ext_addr"
        );

        let usdt_extended_uudt_normal = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x100),
            (PARAM_CAN_RESP_USDT_FORMAT, 0x08 | 0x01), // extended addressing, flow control enabled
            (PARAM_CAN_RESP_USDT_EXT_ADDR, 0xAA),
            (PARAM_CAN_PHYS_REQ_ID, 0x200),
            (PARAM_CAN_RESP_UUDT_ID, 0x100),
            (PARAM_CAN_RESP_UUDT_FORMAT, 0x00), // normal addressing
        ]);
        assert!(
            find_native_mixed_uudt_usdt_collision(&[&[usdt_extended_uudt_normal]]).is_some(),
            "a normal-addressed UUDT id matches any data content, so a frame meant for it can \
             also be captured by an extended-addressed USDT FLOW_CONTROL_FILTER at the same id"
        );
    }

    /// ADR-162 Decision 2: a `CP_CanRespUSDTId` with flow control disabled
    /// (Table B.13 bit 0 clear) never gets a `FLOW_CONTROL_FILTER` installed
    /// by `install_point_to_point_fc_filters`, so it must not participate in
    /// collision detection even at a numerically-identical UUDT id.
    #[test]
    fn find_native_mixed_uudt_usdt_collision_excludes_flow_control_disabled_usdt() {
        let entry = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x100),
            (PARAM_CAN_RESP_USDT_FORMAT, 0x04), // bit 0 (flow control) clear
            (PARAM_CAN_PHYS_REQ_ID, 0x200),
            (PARAM_CAN_RESP_UUDT_ID, 0x100),
        ]);
        assert!(
            find_native_mixed_uudt_usdt_collision(&[&[entry]]).is_none(),
            "a flow-control-disabled USDT id must not participate in collision detection"
        );
    }

    /// ADR-162 Decision 2: a `CP_CanRespUSDTId` entry with no
    /// `CP_CanPhysReqId` never gets a `FLOW_CONTROL_FILTER` installed either
    /// (mirrors `install_point_to_point_fc_filters`'s own `req.is_none()`
    /// warn-and-skip), so it must not participate in collision detection.
    #[test]
    fn find_native_mixed_uudt_usdt_collision_excludes_usdt_with_no_phys_req_id() {
        let entry = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x100),
            (PARAM_CAN_RESP_UUDT_ID, 0x100),
        ]);
        assert!(
            find_native_mixed_uudt_usdt_collision(&[&[entry]]).is_none(),
            "a USDT id with no CP_CanPhysReqId must not participate in collision detection"
        );
    }

    /// ADR-162 Decision 2: a single entry setting both `CP_CanRespUSDTId` and
    /// `CP_CanRespUUDTId` to the identical (eligible) match key is a
    /// self-collision.
    #[test]
    fn find_native_mixed_uudt_usdt_collision_detects_a_self_collision_within_one_entry() {
        let entry = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x7E8),
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_UUDT_ID, 0x7E8),
        ]);
        let collision = find_native_mixed_uudt_usdt_collision(&[&[entry]])
            .expect("identical USDT and UUDT match keys within one entry must collide");
        assert_eq!(collision.id, 0x7E8);
    }

    /// ADR-162 Decision 2 / edge-case-hunter regression: `CanIdFormat.
    /// flow_control_enabled` is NOT part of the match key. A USDT key's bit
    /// is always set (the eligibility gate requires it); a UUDT key's bit is
    /// spec-meaningless and a real client typically leaves it clear. Without
    /// normalizing it out of the comparison, an identical id/width/ext_addr
    /// pair with differing FC bits would silently fail to collide -- exactly
    /// the realistic configuration this check exists to catch.
    #[test]
    fn find_native_mixed_uudt_usdt_collision_ignores_the_flow_control_bit() {
        const EXTENDED_FC_ENABLED: u32 = 0x08 | 0x01; // extended addressing + FC bit set
        const EXTENDED_FC_CLEAR: u32 = 0x08; // extended addressing only, FC bit clear

        let entry = urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x7E8),
            (PARAM_CAN_RESP_USDT_FORMAT, EXTENDED_FC_ENABLED),
            (PARAM_CAN_RESP_USDT_EXT_ADDR, 0xAA),
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
            (PARAM_CAN_RESP_UUDT_ID, 0x7E8),
            (PARAM_CAN_RESP_UUDT_FORMAT, EXTENDED_FC_CLEAR),
            (PARAM_CAN_RESP_UUDT_EXT_ADDR, 0xAA),
        ]);
        let collision = find_native_mixed_uudt_usdt_collision(&[&[entry]])
            .expect("identical id/width/ext_addr must collide regardless of the flow-control bit");
        assert_eq!(collision.id, 0x7E8);
    }

    /// ADR-162 Decision 2: the collision scan pools every table passed in --
    /// a candidate CLL's UUDT id colliding with a SIBLING table's USDT key
    /// must be detected too, not just within a single table.
    #[test]
    fn find_native_mixed_uudt_usdt_collision_detects_a_collision_across_pooled_tables() {
        let candidate = vec![urid_entry(&[(PARAM_CAN_RESP_UUDT_ID, 0x5E8)])];
        let sibling = vec![urid_entry(&[
            (PARAM_CAN_RESP_USDT_ID, 0x5E8),
            (PARAM_CAN_PHYS_REQ_ID, 0x7E0),
        ])];
        assert!(
            find_native_mixed_uudt_usdt_collision(&[candidate.as_slice(), sibling.as_slice()])
                .is_some(),
            "a collision must be detected across pooled tables (candidate + sibling CLL), not \
             just within a single table"
        );
    }

    /// Builds a `J2534Service` with a real device open and a real physical
    /// CAN channel connected through the mock cdylib, plus a single
    /// `cll_handle` already `connected: true` and referencing that channel
    /// (`SharedChannel::ref_count == 1`) -- for exercising
    /// `unstaged_vendor_config_read` against a genuine live `channel_id`
    /// resolvable through `shared_channels`. Mirrors `rpc_misc.rs::
    /// repeat_message_leak_tests::service_with_two_sibling_clls_sharing_a_
    /// channel`'s construction shape (this crate's established pattern for
    /// a hand-built connected CLL + `SharedChannel` pair in a unit test),
    /// trimmed to a single CLL since this fixture only needs one.
    async fn service_with_one_connected_vendor_channel() -> (J2534Service, u32) {
        const CLL_HANDLE: u32 = 1;

        let lib_path = j2534_0404_mock::mock_library_path().expect("mock cdylib should be built");
        let api = j2534_0404::J2534Api0404::from_path(&lib_path).expect("mock cdylib should load");
        let device_id = api.open(None).expect("mock open");
        let channel_id = api
            .connect(device_id, j2534_0404::CAN, 0, 500_000)
            .expect("mock connect");
        let channel_key: ChannelKey = (j2534_0404::CAN, 500_000, 0, 0);

        let mut link = minimal_not_connected_link();
        link.connected = true;
        link.channel_id = Some(channel_id);
        link.channel_key = Some(channel_key);
        let mut logical_links = HashMap::new();
        logical_links.insert(CLL_HANDLE, link);

        let mut shared_channels = HashMap::new();
        let (tx_tx, _tx_rx) = tokio::sync::mpsc::unbounded_channel();
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

        let service = J2534Service {
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
            primitives: Arc::new(Mutex::new(HashMap::new())),
            terminal_cops: Arc::new(Mutex::new(TerminalCopsLedger::default())),
            next_cll_handle: Arc::new(Mutex::new(1)),
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
        };
        (service, CLL_HANDLE)
    }

    /// Regression (Codex review, PR #133, Finding 2): `unstaged_vendor_
    /// config_read` now keeps `shared_channels` locked across the native
    /// `get_config_u32` call (see that function's own doc comment for the
    /// TOCTOU race this closes -- a concurrent disconnect/reconnect
    /// recycling the numeric `channel_id` between resolution and the native
    /// read), mirroring `rpc_misc.rs::rpc_io_ctl_vendor`'s `CllHandle` branch
    /// (`run_vendor_ioctl`), which already holds `shared_channels` across an
    /// `self.api.lock().await`-guarded native call for the identical race
    /// class.
    ///
    /// The actual race (a concurrent disconnect/reconnect landing between
    /// `channel_id` resolution and the native read) is not practically
    /// constructible through this crate's test harness: neither the mock
    /// cdylib nor this crate's `shared_channels` plumbing has a hold hook to
    /// pause `unstaged_vendor_config_read` mid-call so a second, concurrent
    /// task could disconnect/reconnect the same `cll_handle` inside that
    /// window (the same gap `rpc_link.rs::tests::pdu_connect_begun_reflects_
    /// the_connect_in_flight_token`'s own doc comment and `finalize_
    /// connected_link_writes_analog_connect_sync_for_a_new_channel`'s doc
    /// comment both note for their own, differently-shaped races). This test
    /// is the substitute: it drives `unstaged_vendor_config_read` end to end
    /// against a genuinely connected mock channel, proving the new lock
    /// ordering (`shared_channels` held while acquiring `self.api` inside
    /// the awaited native call, then dropped only after that call returns)
    /// neither deadlocks nor breaks the function's own correctness -- the
    /// two concrete risks the fix's own lock-ordering change could otherwise
    /// have introduced.
    #[tokio::test]
    async fn unstaged_vendor_config_read_succeeds_with_shared_channels_held_across_the_native_call()
    {
        const VENDOR_PARAM_ID: u32 = 0x0001_1234;
        let (service, cll_handle) = service_with_one_connected_vendor_channel().await;

        let value = service
            .unstaged_vendor_config_read(cll_handle, ComParamId(VENDOR_PARAM_ID))
            .await
            .expect(
                "a live PassThruGetConfig read on a connected channel must succeed with \
                 shared_channels held across the native call",
            );
        // The mock's GET_CONFIG default for a channel param never SET_CONFIG'd
        // is 0 (channel.params falls back to unwrap_or(0)) -- same default
        // `vendor_comparam_unstaged_connected_reads_live_value_without_staging`
        // (tests/grpc_mock/vendor_passthrough.rs) observes at the RPC level.
        assert_eq!(
            value, 0,
            "an unset vendor ComParamId's live read should report the mock's GET_CONFIG default"
        );
    }
}
