use super::*;
/// Per-protocol ComParam allowlist.
///
/// `check_param_allowed` enforces that `SetComParam` / `GetComParam` only
/// accept param IDs that are relevant to the channel's J2534 protocol.
/// Params that are not in the allowlist are rejected with an emulated
/// `PDU_ERR_COMPARAM_NOT_SUPPORTED` [`state_guard_status`].
///
/// Protocol groupings (J2534 protocol ID → D-PDU protocol family):
///   CAN family    : CAN (0x05), ISO15765 (0x06)
///   KWP family    : ISO9141 (0x03), ISO14230 (0x04)
///   J1850PWM      : J1850PWM (0x02)
///   J1850VPW      : J1850VPW (0x01)
///   SCI family    : SCI_A_ENGINE (0x07), SCI_A_TRANS (0x08),
///                   SCI_B_ENGINE (0x09), SCI_B_TRANS (0x0A),
///                   SCI_MODE (TX-flag value, used for SAE J2610)
///   UART_ECHO_BYTE: PROTOCOL_UART_ECHO_BYTE_PS (0x0000800A, ADR-170/Phase 9)
///                   -- universal params (DATA_RATE/LOOPBACK) plus the ten
///                   minted PARAM_UEB_* timing ComParams (ADR-216), clause
///                   12.3.4.1's closed parameter list
///   HONDA_DIAGH   : PROTOCOL_HONDA_DIAGH_PS (0x0000800B, ADR-174/Phase 10)
///                   -- LOOPBACK/P1_MAX/P3_MIN/P4_MIN only, clause 13.3.3.1's
///                   closed parameter list; narrower than the universal
///                   DATA_RATE/LOOPBACK pair -- clause 13's baud rate is a
///                   fixed 9600bps, not SetComParam-configurable, so this
///                   protocol is checked *before* `is_universal_param` below
///                   rather than after it (contrast UART_ECHO_BYTE, which
///                   relies on the universal check)
///   J1708         : PROTOCOL_J1708_PS (0x0000800D, ADR-175/Phase 11) --
///                   DATA_RATE/LOOPBACK/PARAM_MESSAGE_PRIORITY only, clause
///                   17.3.2.2.1's closed parameter list; a superset of the
///                   universal DATA_RATE/LOOPBACK pair (unlike HONDA_DIAGH),
///                   so this protocol is checked after `is_universal_param`
///                   below, mirroring UART_ECHO_BYTE's own position
///   J1939         : PROTOCOL_J1939_PS/_CH1..128 (0x0000800C/0x9700-0x977F,
///                   ADR-179/Phase 5) -- clause 16.3.3.1.1's general CAN
///                   config surface (DATA_RATE/LOOPBACK/BIT_SAMPLE_POINT/
///                   SYNC_JUMP_WIDTH) plus every PARAM_J1939_* ComParam plus
///                   PARAM_MESSAGE_PRIORITY plus the ISO15765-TP-timer
///                   ComParams this protocol reuses with J1939-specific
///                   defaults (CP_Cr/CP_Cs/CP_Bs/CP_Br, already registered as
///                   PARAM_N_CR/_N_CS/_N_BS/_N_BR, plus CP_T3Max/_T4Max/
///                   _T5Max, the retired-and-reunified native T3_MAX/T4_MAX/
///                   T5_MAX ComParamIds -- ADR-179 Decision 5); a superset of
///                   the universal DATA_RATE/LOOPBACK pair, so this protocol
///                   is checked after `is_universal_param` below, mirroring
///                   J1708's own position
///   ANALOG_IN     : the 32 native PROTOCOL_ANALOG_IN_x ids (ADR-177/Phase
///                   15; revised by ADR-178, then ADR-216) -- allowlist of
///                   the seven project-minted Analog Inputs ComParams
///                   (`PARAM_ANALOG_SAMPLE_RATE`, the four writable
///                   ACTIVE_CHANNELS/SAMPLES_PER_READING/READINGS_PER_MSG/
///                   AVERAGING_METHOD, and the three read-only
///                   SAMPLE_RESOLUTION/INPUT_RANGE_LOW/INPUT_RANGE_HIGH,
///                   ADR-216), not even DATA_RATE/LOOPBACK; checked before
///                   `is_universal_param` below like HONDA_DIAGH, since its
///                   closed list is narrower than the universal pair too.
///                   This still closes off CP_TesterPresentSendType by
///                   construction, the same mechanism ADR-170 Decision 3
///                   established for UART Echo Byte's own closed list --
///                   every clause 10 native SET_CONFIG/GET_CONFIG
///                   acquisition parameter now has a ComParam counterpart
///                   (ADR-216 closes the remaining seven)
///   ETHERNET_NDIS : PROTOCOL_ETHERNET_NDIS (0x00008013, ADR-194/Phase 16) --
///                   allowlist of exactly ONE param, `PARAM_NDIS_PIN_OPTION`
///                   (the project-minted `CP_NdisPinOption`), not even
///                   DATA_RATE/LOOPBACK; checked before `is_universal_param`
///                   below like HONDA_DIAGH/ANALOG_IN, since clause 24
///                   defines no native ComParam-shaped concept at all
///   Unknown       : allow everything (forward-compat / custom protocols) except
///                   PARAM_ANALOG_SAMPLE_RATE, which is rejected here too
///                   (ADR-178, Codex review PR #70 round 3) -- it is only ever
///                   read/applied for a resolved ANALOG_IN protocol id, so
///                   letting it through for an unrecognized protocol would
///                   let a client stage it there with no effect, while
///                   GetComParam kept reporting it as set
///   Vendor (`>= 0x10000`): allowed on every protocol, unconditionally, ahead
///                   of every check above (`ComParamId::is_vendor()`,
///                   ADR-219 amendment) -- SAE J2534-1's tool-manufacturer
///                   reservation has no per-protocol qualification, so no
///                   SAE-derived allowlist can answer for it; the vendor
///                   DLL's own SET_CONFIG/GET_CONFIG result is the authority
use tonic::{Code, Status};
use vci_service_interface::PduError;

use crate::error::state_guard_status;

/// Returns `Ok(())` if `param_id` may be get/set on a channel with
/// `protocol`, or an emulated `PDU_ERR_COMPARAM_NOT_SUPPORTED`
/// [`state_guard_status`] rejection if not -- this is a real D-PDU-level
/// state rejection (ISO 22900-2), not request-shape validation, so it
/// carries an `ErrorDetail` like this crate's other state guards.
/// `last_error` should be the value already read from the CLL in scope at
/// the call site (see `state_guard_status`'s own contract).
pub(super) fn check_param_allowed(
    protocol: ChannelProtocol,
    param_id: ComParamId,
    last_error: Option<TrackedError>,
) -> Result<(), Status> {
    if is_param_allowed(protocol, param_id) {
        Ok(())
    } else {
        Err(state_guard_status(
            Code::InvalidArgument,
            format!(
                "ComParam {:#010x} is not supported for protocol {:#010x}",
                param_id.0,
                protocol.value()
            ),
            PduError::PduErrComparamNotSupported,
            last_error,
        ))
    }
}

fn is_param_allowed(protocol: ChannelProtocol, param_id: ComParamId) -> bool {
    // ADR-219 amendment (design-advisor decision, same-day follow-up to the
    // original ADR-219 landing): a vendor ComParamId (`param_id.is_vendor()`,
    // SAE J2534-1 §7.2.14.3/§7.3.2's tool-manufacturer-reserved
    // `>= 0x10000` range) is admitted unconditionally here, for every
    // protocol, ahead of every protocol-specific branch below including the
    // closed-list ones (Honda DIAG-H, Analog Inputs, Ethernet_NDIS). ADR-028
    // defines this allowlist as answering "may a D-PDU client reference this
    // ComParam on this protocol," derived from SAE/ISO parameter tables --
    // but J2534-1's tool-manufacturer reservation has no per-protocol
    // qualification, so no SAE clause can answer that question for a vendor
    // id; the vendor DLL's own `SET_CONFIG`/`GET_CONFIG` result is the only
    // authority (see `to_j2534_config_id`'s identity passthrough, which
    // always forwards a vendor id, unlike the closed lists' silent-no-op
    // exclusions this allowlist otherwise enforces). Relies on the
    // invariant documented on `ComParamId::is_vendor()`: no service-minted
    // `ComParamId` may ever be `>= 0x10000`, or this early return would
    // silently shadow that id's own protocol-specific exclusion.
    if param_id.is_vendor() {
        return true;
    }
    // SAE J2534-2 clause 13 Honda DIAG-H Protocol (ADR-174/Phase 10):
    // deliberately checked *before* the universal-param short-circuit below,
    // unlike every other protocol-specific branch in this function (which
    // all run after it, relying on `is_universal_param` to already cover
    // DATA_RATE/LOOPBACK for them). Clause 13.3.3.1's closed parameter list
    // is narrower than that: `DATA_RATE` is excluded because clause 13
    // Table 40's baud rate is a fixed 9600bps, not `SetComParam`-configurable
    // at all -- the first protocol in this codebase whose allowlist needs to
    // reject a param `is_universal_param` would otherwise allow
    // unconditionally for every protocol.
    if protocol.j2534_protocol_id() == j2534_0404::PROTOCOL_HONDA_DIAGH_PS {
        return is_honda_diagh_param(param_id);
    }
    // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15; revised by
    // ADR-178): deliberately checked *before* the universal-param
    // short-circuit below, mirroring Honda DIAG-H's own precedent just
    // above -- this protocol's allowlist is not just narrower than
    // DATA_RATE/LOOPBACK, it allows exactly one param,
    // `PARAM_ANALOG_SAMPLE_RATE` (`CP_AnalogSampleRate`, ADR-178), and
    // rejects everything else including DATA_RATE/LOOPBACK. Clause 10
    // defines no native baud rate, loopback, or any other ComParam-shaped
    // concept; SAMPLE_RATE alone was promoted out of clause 10's native
    // SET_CONFIG/GET_CONFIG acquisition parameters into a ComParam because
    // its zero default disables the acquisition subsystem entirely and its
    // former request-field carrier (`CreateComLogicalLinkRequest.analog_sample_rate`)
    // was removed by ADR-178; it is staged via SetComParam and resolved at
    // ConnectComLogicalLink time. This still closes off
    // `CP_TesterPresentSendType` by construction, the same mechanism
    // ADR-170 Decision 3 established for UART Echo Byte's own closed list.
    // `resources::is_analog_in_protocol_id` takes the resolved native id
    // (not `protocol.value()`), matching every other identity check in this
    // file/module -- see its own doc comment. Because this branch returns
    // unconditionally for every ANALOG_IN protocol id, no other family's
    // branch below is ever reached for it, so `PARAM_ANALOG_SAMPLE_RATE` is
    // unreachable/rejected for every non-ANALOG_IN protocol by construction.
    // ADR-216: the allowlist widens from the single `PARAM_ANALOG_SAMPLE_RATE`
    // entry to the seven Analog Inputs ComParams this ADR mints -- the four
    // writable ones (`ACTIVE_CHANNELS`/`SAMPLES_PER_READING`/
    // `READINGS_PER_MSG`/`AVERAGING_METHOD`) plus the three read-only
    // capability ones (`SAMPLE_RESOLUTION`/`INPUT_RANGE_LOW`/
    // `INPUT_RANGE_HIGH`). The three read-only ones are allowed here for
    // `GetComParam` reach (they must be readable, populated by the
    // connect-time readback, ADR-216 Decision item 6) even though
    // `SetComParam` never actually accepts them -- that rejection is
    // enforced separately, unconditionally, at `rpc_set_com_param`'s own
    // Unum32 arm (ADR-216 Decision item 5), not by narrowing this allowlist.
    if resources::is_analog_in_protocol_id(protocol.j2534_protocol_id()) {
        return matches!(
            param_id,
            PARAM_ANALOG_SAMPLE_RATE
                | PARAM_ANALOG_ACTIVE_CHANNELS
                | PARAM_ANALOG_SAMPLES_PER_READING
                | PARAM_ANALOG_READINGS_PER_MSG
                | PARAM_ANALOG_AVERAGING_METHOD
                | PARAM_ANALOG_SAMPLE_RESOLUTION
                | PARAM_ANALOG_INPUT_RANGE_LOW
                | PARAM_ANALOG_INPUT_RANGE_HIGH
        );
    }
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): deliberately
    // checked *before* the universal-param short-circuit below, mirroring
    // Honda DIAG-H's/Analog Inputs' own precedent just above -- this
    // protocol's allowlist is not just narrower than DATA_RATE/LOOPBACK, it
    // allows exactly one param, `PARAM_NDIS_PIN_OPTION` (`CP_NdisPinOption`,
    // ADR-194), and rejects everything else including DATA_RATE/LOOPBACK.
    // Clause 24 defines no baud rate, loopback, or any other native
    // ComParam-shaped concept -- the payload traffic itself is entirely
    // out-of-band (ADR-194 Context), so no per-tick tester-present/timing
    // ComParam has any meaning here either.
    if protocol.j2534_protocol_id() == j2534_0404::PROTOCOL_ETHERNET_NDIS {
        return param_id == PARAM_NDIS_PIN_OPTION;
    }
    if is_universal_param(param_id) {
        return true;
    }
    // PDU_PC_UNIQUE_ID class params carry per-ECU addressing that must vary
    // independently per entry on a shared channel; ISO 22900-2 §9.3.3.6
    // mandates they are managed exclusively through GetUniqueRespIdTable /
    // SetUniqueRespIdTable, not GetComParam / SetComParam.
    if is_unique_id_param(protocol, param_id) {
        return false;
    }
    if protocol.is_can_family() {
        return is_can_param(param_id);
    }
    if protocol.is_kwp_family() {
        if is_iso14230_only_structfield_param(param_id) {
            // edge-case-hunter finding, PR #22 close-out: was `protocol ==
            // ChannelProtocol::ISO14230` (exact identity) -- ADR-150's
            // round-3 correction widened `TimingChangeConfig::from_params`'s
            // own KWP gate to `kwp_access_timing_applies()` (bare ISO14230
            // plus `ISO_14230_3_ON_ISO_14230_2`/`ISO_15031_5_ON_ISO_14230_4`)
            // but this allow-list gate for the SAME three ComParams
            // (`CP_AccessTiming_Ecu`/`CP_AccessTimingOverride`/`CP_ExtendedTiming`)
            // was left on exact identity, leaving the mechanism silently
            // active on the two extended protocols while a client could
            // never `SetComParam`/`GetComParam` either the override or the
            // recorded-result ComParam for it. Reuses the same predicate as
            // the single source of truth instead of a second hardcoded
            // check, so the two can no longer drift apart independently.
            return protocol.kwp_access_timing_applies();
        }
        return is_kwp_param(param_id);
    }
    if protocol.j2534_protocol_id() == j2534_0404::J1850PWM {
        return is_j1850pwm_param(param_id);
    }
    if protocol.j2534_protocol_id() == j2534_0404::J1850VPW {
        return is_j1850vpw_param(param_id);
    }
    if protocol.is_sci_family() {
        return is_sci_param(param_id);
    }
    // SAE J2534-2 clause 12 UART Echo Byte Protocol (ADR-170/Phase 9): an
    // explicit exact-identity check, mirroring the J1850PWM/J1850VPW arms
    // above -- checked before the "Unknown protocol — allow" fallback below,
    // since without it a bare `ChannelProtocol::UART_ECHO_BYTE_PS` would fall
    // through to that fallback and make every ComParam settable/gettable on
    // it, contradicting clause 12.3.4.1's closed parameter list (only
    // DATA_RATE/LOOPBACK, already covered by `is_universal_param` above).
    // This also makes `CP_TesterPresentSendType` unreachable for this
    // protocol by construction, satisfying clause 12.3.3.3's periodic
    // tester-present exclusion with no separate runtime guard (ADR-170
    // Decision 3/rejected-alternatives). ADR-216: the ten `PARAM_UEB_*`
    // timing ComParams (clause 12.3.4.1's `UEB_T0_MIN`-`UEB_T9_MIN`) join
    // this allowlist too, ORed alongside the universal DATA_RATE/LOOPBACK
    // pair -- `protocol` here is this CLL's already-normalized
    // `ChannelProtocol` (the family identity, `UART_ECHO_BYTE_PS` even for a
    // `_CHx`-connected link -- see `ChannelProtocol::UART_ECHO_BYTE_PS`'s own
    // doc comment), so no family-wide predicate is needed at this allowlist
    // layer the way `comparam_id.rs`'s translation arm needs one for the raw
    // `hw_protocol_id`.
    if protocol.j2534_protocol_id() == j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS {
        return is_universal_param(param_id) || is_ueb_timing_param(param_id);
    }
    // SAE J2534-2 clause 17 SAE J1708 Protocol (ADR-175/Phase 11): an
    // explicit exact-identity check, mirroring the UART Echo Byte arm just
    // above -- checked before the "Unknown protocol — allow" fallback below.
    // Unlike Honda DIAG-H, J1708 does not need to preempt
    // `is_universal_param`: its allowlist already includes everything
    // `is_universal_param` would allow (`DATA_RATE`/`LOOPBACK`) plus one more
    // param (`PARAM_MESSAGE_PRIORITY`), so this arm's own position after the
    // short-circuit is harmless -- it only ever narrows a superset check to
    // an identical-or-wider one for this protocol.
    if protocol.j2534_protocol_id() == j2534_0404::PROTOCOL_J1708_PS {
        return is_j1708_param(param_id);
    }
    // SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5): an
    // explicit check, mirroring the J1708 arm just above -- checked before
    // the "Unknown protocol — allow" fallback below. Like J1708, J1939 does
    // not need to preempt `is_universal_param`: its allowlist already
    // includes everything `is_universal_param` would allow
    // (`DATA_RATE`/`LOOPBACK`) plus a much wider set, so this arm's own
    // position after the short-circuit is harmless. Uses
    // `resources::is_j1939_protocol_id` (not exact identity) so the
    // `_CH1..128` range is covered too, even though no resource-table row or
    // connect path reaches one yet (Additional Channels stay deferred this
    // phase) -- matching `is_j1939_protocol_id`'s own doc comment reasoning.
    if resources::is_j1939_protocol_id(protocol.j2534_protocol_id()) {
        return is_j1939_param(param_id);
    }
    // SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a,
    // extended by ADR-190/Phase 7 Stage 7b, then ADR-192/Phase 7 Stage 7c):
    // an explicit check, mirroring the J1939 arm just above -- checked before
    // the "Unknown protocol — allow" fallback below. Like J1708/J1939, TP2.0
    // does not need to preempt `is_universal_param`: its allowlist already
    // includes everything `is_universal_param` would allow (`DATA_RATE`/
    // `LOOPBACK`) plus the CAN bit-timing pair and the nine minted
    // `PARAM_TP20_*` ComParams (the five Stage 7a active-connection ones,
    // Stage 7b's two passive-connection ones, and Stage 7c's two broadcast
    // ones), so this arm's own position after the short-circuit is harmless.
    if resources::is_tp2_0_protocol_id(protocol.j2534_protocol_id()) {
        return is_tp20_param(param_id);
    }
    // HONDA_DIAGH_PS (ADR-174/Phase 10) is NOT handled here -- its allowlist
    // excludes `DATA_RATE`, which `is_universal_param` would otherwise let
    // through unconditionally at the top of this function, so it is checked
    // there instead, before that short-circuit.
    // SAE J2534-2 clause 10 Analog Inputs (ADR-178, Codex review PR #70
    // round 3): `PARAM_ANALOG_SAMPLE_RATE` is excluded from the "Unknown
    // protocol — allow" fallback below explicitly, unlike every other
    // ComParam this function's unknown-protocol branch lets through. The
    // ANALOG_IN branch above (`resources::is_analog_in_protocol_id`) only
    // ever returns for a resolved native `PROTOCOL_ANALOG_IN_x` id -- a CLL
    // on any other protocol, including an unrecognized vendor/custom raw id
    // that reaches this fallback, never touches that branch at all. Without
    // this exclusion, `SetComParam(CP_AnalogSampleRate, nonzero)` would
    // silently succeed on such a CLL (the fallback's own `true`), yet
    // `rpc_connect_com_logical_link`'s snapshot block only ever reads/applies
    // this value when `is_analog_in_protocol_id` is true -- so the staged
    // value would be silently ignored at connect time while `GetComParam`
    // kept reporting it as set, regressing the removed
    // `CreateComLogicalLinkRequest.analog_sample_rate` field's own explicit
    // rejection of a nonzero value on every non-analog resource (ADR-177).
    // ADR-216: the same exclusion extends to the six other Analog Inputs
    // ComParams this ADR mints -- none of them has any effect (forwarded or
    // readback) outside a resolved ANALOG_IN protocol id either, so letting
    // any of them through the fallback would be the identical silent-no-op
    // hazard.
    if matches!(
        param_id,
        PARAM_ANALOG_SAMPLE_RATE
            | PARAM_ANALOG_ACTIVE_CHANNELS
            | PARAM_ANALOG_SAMPLES_PER_READING
            | PARAM_ANALOG_READINGS_PER_MSG
            | PARAM_ANALOG_AVERAGING_METHOD
            | PARAM_ANALOG_SAMPLE_RESOLUTION
            | PARAM_ANALOG_INPUT_RANGE_LOW
            | PARAM_ANALOG_INPUT_RANGE_HIGH
    ) {
        return false;
    }
    // SAE J2534-2 clause 19 TP2.0 (ADR-188/Phase 7 Stage 7a; extended by
    // ADR-190/Phase 7 Stage 7b's two minted passive-connection ComParams,
    // then ADR-192/Phase 7 Stage 7c's two minted broadcast ComParams): the
    // nine minted `PARAM_TP20_*` ComParams are excluded from the "Unknown
    // protocol — allow" fallback below, the same `PARAM_ANALOG_SAMPLE_RATE`
    // exclusion mechanism just above -- they are only ever read by
    // `events_tp20_connection.rs`/`service.rs::tp20_broadcast_address`/the
    // generic ComParam-to-`SET_CONFIG` pipeline for a resolved `TP2_0_PS`
    // link, so allowing them on an unrelated/unrecognized protocol would let
    // a client stage a value with no effect while `GetComParam` kept
    // reporting it as set.
    if matches!(
        param_id,
        PARAM_TP20_CHANNEL_SETUP_CAN_ID
            | PARAM_TP20_DESTINATION_ADDRESS
            | PARAM_TP20_TX_ID_PROPOSAL
            | PARAM_TP20_RX_ID_PROPOSAL
            | PARAM_TP20_APPLICATION_TYPE
            | PARAM_TP20_PASSIVE_IDENTIFIER
            | PARAM_TP20_PASSIVE_RX_ID
            | PARAM_TP20_BROADCAST_ADDRESS
            | PARAM_TP20_BROADCAST_INTERVAL
    ) {
        return false;
    }
    // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16):
    // `PARAM_NDIS_PIN_OPTION` is excluded from the "Unknown protocol —
    // allow" fallback below, the same `PARAM_ANALOG_SAMPLE_RATE`/
    // `PARAM_TP20_*` exclusion mechanism above -- the ETHERNET_NDIS branch
    // above only ever returns for a resolved `PROTOCOL_ETHERNET_NDIS` id, so
    // without this exclusion `SetComParam(CP_NdisPinOption, ...)` would
    // silently succeed on an unrelated/unrecognized protocol (the fallback's
    // own `true`) while `rpc_link.rs::connect_flags` only ever reads it for
    // an Ethernet_NDIS connect -- the staged value would be silently ignored
    // at connect time while `GetComParam` kept reporting it as set.
    if param_id == PARAM_NDIS_PIN_OPTION {
        return false;
    }
    // SAE J2534-2 clause 12.3.4.1 UART Echo Byte (ADR-216): the ten
    // `PARAM_UEB_*` timing ComParams are excluded from the "Unknown protocol
    // — allow" fallback below, the same exclusion mechanism as every other
    // minted ComParam above -- the UEB branch above only ever returns for a
    // resolved `PROTOCOL_UART_ECHO_BYTE_PS` id, so without this exclusion a
    // client could stage one on an unrelated/unrecognized protocol with no
    // effect (`comparam_id.rs`'s translation arm only forwards them for a
    // UART Echo Byte family `hw_protocol_id`) while `GetComParam` kept
    // reporting it as set.
    if is_ueb_timing_param(param_id) {
        return false;
    }
    // Unknown protocol — allow to avoid rejecting custom/future protocols.
    true
}

/// `CP_AccessTiming_Ecu`/`CP_AccessTimingOverride`/`CP_ExtendedTiming`: ISO
/// 22900-2's own default-by-protocol tables for these three ComParams list
/// only ISO_14230_2/ISO_14230_4, never ISO_9141_2 -- narrower than this
/// file's `is_kwp_family` grouping (which also covers ISO9141, a protocol
/// with no equivalent Access Timing Parameter service). `is_param_allowed`
/// checks this before falling through to `is_kwp_param` so these three stay
/// gated to `ChannelProtocol::ISO14230` specifically (ADR-146).
fn is_iso14230_only_structfield_param(p: ComParamId) -> bool {
    matches!(
        p,
        PARAM_ACCESS_TIMING_ECU | PARAM_ACCESS_TIMING_OVERRIDE | PARAM_EXTENDED_TIMING
    )
}

// ── Universal params (all protocols) ────────────────────────────────────────

fn is_universal_param(p: ComParamId) -> bool {
    matches!(
        p,
        // CP_Baudrate (S for all) — D-PDU standard
        ComParamId(j2534_0404::DATA_RATE)
        // J2534-specific loopback; not a D-PDU ComParam but universally applicable
        | ComParamId(j2534_0404::LOOPBACK)
    )
}

/// SAE J2534-2 clause 12.3.4.1 UART Echo Byte's ten minted timing ComParams
/// (`CP_UebT0Min`-`CP_UebT9Min`, ADR-216) -- project-invented, no ISO 22900-2
/// source, joining `is_universal_param`'s DATA_RATE/LOOPBACK pair as this
/// protocol's own allowlist addition (ADR-170 Decision 3's original closed
/// list plus this ADR's own addition).
fn is_ueb_timing_param(p: ComParamId) -> bool {
    matches!(
        p,
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
    )
}

/// SAE J2534-2 clause 13.3.3.1's closed parameter list for Honda DIAG-H
/// (ADR-174/Phase 10): `LOOPBACK`, `P1_MAX`, `P3_MIN`, `P4_MIN`. Deliberately
/// excludes `DATA_RATE` -- clause 13 Table 40's baud rate is a fixed
/// 9600bps, not `SetComParam`-configurable, unlike every other protocol's
/// `DATA_RATE` (which `is_universal_param` covers). Also excludes
/// `CP_InitializationSettings` (`PARAM_INIT_SETTINGS`): no init sequence
/// ever runs for this protocol regardless of this param's value (clause
/// 13.3.1, `rpc_primitive`'s deliberate `is_kline == false` classification),
/// so allowing it would let a caller set a value carrying no real
/// information (ADR-174 Decision 4, mirroring ADR-170 Decision 3's identical
/// reasoning for UART Echo Byte).
fn is_honda_diagh_param(p: ComParamId) -> bool {
    matches!(
        p,
        ComParamId(j2534_0404::LOOPBACK)
            | ComParamId(j2534_0404::P1_MAX)
            | ComParamId(j2534_0404::P3_MIN)
            | ComParamId(j2534_0404::P4_MIN)
    )
}

/// SAE J2534-2 clause 17.3.2.2.1's closed parameter list for SAE J1708
/// (ADR-175/Phase 11): `DATA_RATE`/`LOOPBACK` (unlike Honda DIAG-H, J1708's
/// baud rate genuinely is `SetComParam`-configurable -- clause 17.2.2 gives a
/// minimum-support default, not a fixed value) plus `PARAM_MESSAGE_PRIORITY`
/// (`CP_MessagePriority`, clause 17.4.5 -- the ComParam backing the new
/// `MSG_PRIORITY_VALUE` TxFlags mechanism, `ComParamSet::msg_priority_tx_flags`).
fn is_j1708_param(p: ComParamId) -> bool {
    matches!(
        p,
        ComParamId(j2534_0404::DATA_RATE)
            | ComParamId(j2534_0404::LOOPBACK)
            | PARAM_MESSAGE_PRIORITY
    )
}

/// SAE J2534-2 clause 16 SAE J1939 Protocol (ADR-179/Phase 5): clause
/// 16.3.3.1.1's general CAN config surface (`DATA_RATE`/`LOOPBACK`/
/// `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH`) plus every `PARAM_J1939_*` ComParam
/// `service_params.rs` mints, `PARAM_MESSAGE_PRIORITY` (already shared with
/// J1850/J1708), and the ISO15765-TP-timer ComParams this protocol reuses
/// with J1939-specific defaults per ADR-179 Decision 5's table: `CP_Cr`/
/// `CP_Cs`/`CP_Bs`/`CP_Br` (already `PARAM_N_CR`/`_N_CS`/`_N_BS`/`_N_BR`)
/// plus `CP_T3Max`/`_T4Max`/`_T5Max`, now the retired-and-reunified native
/// `T3_MAX`/`T4_MAX`/`T5_MAX` ComParamIds -- see `service_params.rs`'s own
/// doc comment on why these three are no longer separate D-PDU shortnames
/// (ADR-179 closes the shortname-collision gap PR #71 left open this way).
///
/// `PARAM_J1939_SOURCE_ADDRESS` (`CP_J1939SourceAddress`) is deliberately
/// absent (ADR-184): it is now `PDU_PC_UNIQUE_ID` class for `J1939_PS`
/// (`J1939_UNIQUE_ID_UNUM32`) and is rejected by `is_param_allowed`'s
/// `is_unique_id_param` gate before this function is ever reached, same as
/// the CAN-family `CP_Can*`/`CP_EcuRespSourceAddress`/etc. omissions above.
/// `PARAM_J1939_SOURCE_NAME` (`CP_J1939SourceName`) stays listed below --
/// deliberately NOT `PDU_PC_UNIQUE_ID` class this pass (an accepted P3
/// residual, `docs/implementation-notes.md`), so it remains directly
/// settable here.
fn is_j1939_param(p: ComParamId) -> bool {
    matches!(
        p,
        ComParamId(j2534_0404::DATA_RATE)
            | ComParamId(j2534_0404::LOOPBACK)
            | ComParamId(j2534_0404::BIT_SAMPLE_POINT)
            | ComParamId(j2534_0404::SYNC_JUMP_WIDTH)
            // CP_TesterSourceAddress (alias for NODE_ADDRESS, names.rs) --
            // ADR-179 Decision 3: the address-claim state machine writes
            // the claimed address back here so the client can read it via
            // GetComParam after a successful StartComm (ISO 22900-2's own
            // CP_TesterSourceAddress text). Omitted from this allowlist in
            // the initial pass -- the claim loop could still write it (a
            // Working/Active insert has no allowlist gate of its own), but
            // GetComParam/SetComParam themselves rejected it, breaking the
            // client-readback contract ADR-179 specifies.
            | ComParamId(j2534_0404::NODE_ADDRESS)
            | PARAM_MESSAGE_PRIORITY
            | PARAM_N_CR
            | PARAM_N_CS
            | PARAM_N_BS
            | PARAM_N_BR
            | ComParamId(j2534_0404::T3_MAX)
            | ComParamId(j2534_0404::T4_MAX)
            | ComParamId(j2534_0404::T5_MAX)
            | PARAM_J1939_ADDR_CLAIM_TIMEOUT
            | PARAM_J1939_ADDR_NEG_RULE
            | PARAM_J1939_DATA_PAGE
            | PARAM_J1939_MAX_PACKET_TX
            | PARAM_J1939_PDU_FORMAT
            | PARAM_J1939_PDU_SPECIFIC
            | PARAM_J1939_TARGET_ADDRESS
            | PARAM_J1939_PREFERRED_ADDRESS
            | PARAM_J1939_PREFERRED_ADDRESS_ECU
            | PARAM_J1939_NAME
            | PARAM_J1939_NAME_ECU
            | PARAM_J1939_SOURCE_NAME
            | PARAM_J1939_TARGET_NAME
    )
}

/// SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a; extended
/// by ADR-190/Phase 7 Stage 7b, then ADR-192/Phase 7 Stage 7c): clause
/// 19.3.1's general CAN config surface (`DATA_RATE`/`LOOPBACK`/
/// `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH`) plus the nine minted `PARAM_TP20_*`
/// ComParams `service_params.rs` mints -- the five Stage 7a active-connection
/// ones (setup CAN ID, destination address, TX-ID/RX-ID proposal, application
/// type -- Table 78's connection-request fields), Stage 7b's two
/// passive-connection ones (identifier, RX-ID passive -- Table 77's
/// passive-listener enablers), and Stage 7c's two broadcast ones (broadcast
/// address, broadcast interval -- clause 19.3.2.2/Table 77's `T_BR_INT`).
/// `CP_TesterPresentSendType` is deliberately excluded (ADR-188 Decision item
/// 4, the UART Echo Byte Decision 3 exclusion mechanism, ADR-170): clause
/// 19.3.1 requires the device to autonomously maintain an established
/// connection, so no client-driven periodic keep-alive concept exists for
/// this protocol.
fn is_tp20_param(p: ComParamId) -> bool {
    matches!(
        p,
        ComParamId(j2534_0404::DATA_RATE)
            | ComParamId(j2534_0404::LOOPBACK)
            | ComParamId(j2534_0404::BIT_SAMPLE_POINT)
            | ComParamId(j2534_0404::SYNC_JUMP_WIDTH)
            | PARAM_TP20_CHANNEL_SETUP_CAN_ID
            | PARAM_TP20_DESTINATION_ADDRESS
            | PARAM_TP20_TX_ID_PROPOSAL
            | PARAM_TP20_RX_ID_PROPOSAL
            | PARAM_TP20_APPLICATION_TYPE
            | PARAM_TP20_PASSIVE_IDENTIFIER
            | PARAM_TP20_PASSIVE_RX_ID
            | PARAM_TP20_BROADCAST_ADDRESS
            | PARAM_TP20_BROADCAST_INTERVAL
    )
}

// ── CAN family (CAN, ISO15765) ───────────────────────────────────────────────

fn is_can_param(p: ComParamId) -> bool {
    matches!(
        p,
        // ── Physical layer (J2534 native) ─────────────────────────────────────
        // CP_BitSamplePoint (S,T): forwarded to hardware via BIT_SAMPLE_POINT
        ComParamId(j2534_0404::BIT_SAMPLE_POINT)
        // CP_SyncJumpWidth (S,T): forwarded via SYNC_JUMP_WIDTH
        | ComParamId(j2534_0404::SYNC_JUMP_WIDTH)
        // J1939 tester source address
        | ComParamId(j2534_0404::NODE_ADDRESS)
        // ISO 15765-2 flow-control (ISO15765 protocol only, valid for CAN family)
        | ComParamId(j2534_0404::ISO15765_BS)
        | ComParamId(j2534_0404::ISO15765_STMIN)
        | ComParamId(j2534_0404::BS_TX)
        | ComParamId(j2534_0404::STMIN_TX)
        | ComParamId(j2534_0404::ISO15765_WFT_MAX)

        // ── Physical layer (service-level) ────────────────────────────────────
        // CP_BitSamplePoint_Ecu (S,E): stored, not forwarded (no J2534 equiv)
        | PARAM_BIT_SAMPLE_POINT_ECU
        // CP_SamplesPerBit (S,T): stored, not forwarded (no J2534 equiv)
        | PARAM_SAMPLES_PER_BIT
        // CP_SamplesPerBit_Ecu (S,E): stored, not forwarded
        | PARAM_SAMPLES_PER_BIT_ECU
        // CP_SyncJumpWidth_Ecu (S,E): stored, not forwarded
        | PARAM_SYNC_JUMP_WIDTH_ECU
        // CP_ListenOnly (O): stored, not forwarded
        | PARAM_LISTEN_ONLY
        // CP_CanBaudrateRecord (O,T): stored as Bytefield
        | PARAM_CAN_BAUDRATE_RECORD
        // CP_TerminationType (O,T): stored, not forwarded
        | PARAM_TERMINATION_TYPE
        // CP_TerminationType_Ecu (O,E): stored, not forwarded
        | PARAM_TERMINATION_TYPE_ECU

        // ── Transport: ISO 15765-2 frame timing ──────────────────────────────
        | PARAM_N_AR | PARAM_N_AR_ECU
        | PARAM_N_AS | PARAM_N_AS_ECU
        | PARAM_N_BR | PARAM_N_BR_ECU
        | PARAM_N_BS | PARAM_N_BS_ECU
        | PARAM_N_CR | PARAM_N_CR_ECU
        | PARAM_N_CS | PARAM_N_CS_ECU
        | PARAM_ST_MIN_ECU
        | PARAM_BLOCK_SIZE_ECU
        | PARAM_J1939_ADDR_CLAIM_TIMEOUT
        | PARAM_REPEAT_REQ_COUNT_TRANS

        // ── Transport: CAN addressing ─────────────────────────────────────────
        // CP_Can{PhysReq,RespUSDT,RespUUDT}{Id,Format,ExtAddr} are
        // PDU_PC_UNIQUE_ID class (see CAN_UNIQUE_ID_UNUM32 below) and are
        // rejected by is_param_allowed before reaching this list; only
        // CP_CanFuncReq* (a shared functional address, not per-ECU) belongs here.
        | PARAM_CAN_FUNC_REQ_EXT_ADDR
        | PARAM_CAN_FUNC_REQ_FORMAT
        | PARAM_CAN_FUNC_REQ_ID
        | PARAM_CAN_DATA_SIZE_OFFSET
        | PARAM_CAN_FILLER_BYTE
        | PARAM_CAN_FILLER_BYTE_HANDLING
        | PARAM_CAN_FIRST_CF_VALUE

        // ── Transport: ECU addressing / COM ──────────────────────────────────
        // CP_EcuRespSourceAddress, CP_FuncRespFormatPriorityType,
        // CP_FuncRespTargetAddr, CP_PhysRespFormatPriorityType, and
        // CP_MidRespId are PDU_PC_UNIQUE_ID class (see CAN_UNIQUE_ID_UNUM32
        // below) and are rejected by is_param_allowed before reaching this list.
        | PARAM_FUNC_REQ_FORMAT_PRIORITY
        | PARAM_FUNC_REQ_TARGET_ADDR
        | PARAM_PHYS_REQ_FORMAT_PRIORITY
        | PARAM_PHYS_REQ_TARGET_ADDR
        | PARAM_REQUEST_ADDR_MODE
        | PARAM_FILLER_BYTE
        | PARAM_FILLER_BYTE_HANDLING
        | PARAM_FILLER_BYTE_LENGTH
        | PARAM_SEND_REMOTE_FRAME
        | PARAM_TP_CONNECTION_MGMT
        | PARAM_MESSAGE_PRIORITY
        | PARAM_MID_REQ_ID

        // ── Transport: J1939 ─────────────────────────────────────────────────
        // CP_J1939SourceAddress / CP_J1939SourceName are absent from this
        // CAN-family list -- neither ever applied to a plain CAN/ISO15765
        // channel; both are J1939-only ComParams (`is_j1939_param` above
        // handles the actual `J1939_PS` dispatch, `resources::
        // is_j1939_protocol_id`-gated). Previously (pre-ADR-184) both were
        // misclassified as CAN-family PDU_PC_UNIQUE_ID params in
        // CAN_UNIQUE_ID_UNUM32/CAN_UNIQUE_ID_BYTES, a placement that was
        // never actually reachable (`unique_id_params` only ever dispatches
        // there for `CAN`/`ISO15765`, and `J1939_PS.is_can_family() ==
        // false`) -- see those lists' own doc comments for where each
        // param's classification actually lives now.
        | PARAM_J1939_ADDR_NEG_RULE
        | PARAM_J1939_DATA_PAGE
        | PARAM_J1939_MAX_PACKET_TX
        | PARAM_J1939_PDU_FORMAT
        | PARAM_J1939_PDU_SPECIFIC
        | PARAM_J1939_TARGET_ADDRESS
        | PARAM_INIT_SETTINGS
        | PARAM_J1939_PREFERRED_ADDRESS
        | PARAM_J1939_PREFERRED_ADDRESS_ECU
        | PARAM_J1939_NAME
        | PARAM_J1939_NAME_ECU
        | PARAM_J1939_TARGET_NAME

        // ── Application: Tester Present ──────────────────────────────────────
        | PARAM_TESTER_PRESENT_MSG
        | PARAM_TESTER_PRESENT_INTERVAL_US
        | PARAM_TESTER_PRESENT_ADDR_MODE
        | PARAM_TESTER_PRESENT_EXP_POS_RESP
        | PARAM_TESTER_PRESENT_EXP_NEG_RESP
        | PARAM_TESTER_PRESENT_HANDLING
        | PARAM_TESTER_PRESENT_REQ_RSP
        | PARAM_TESTER_PRESENT_SEND_TYPE
        | PARAM_TESTER_PRESENT_TIME_ECU

        // ── Application: Timing ──────────────────────────────────────────────
        | PARAM_CYCLIC_RESP_TIMEOUT
        // CP_P2Min / CP_P2Max: stored service-level, never forwarded to
        // J2534 SET_CONFIG on the CAN family (see comparam_id.rs).
        // CP_P2Max is the CoptSendrecv response window (ADR-053); both are
        // pre-seeded by the CAN bustype defaults, so they must be
        // get/settable here too.
        | ComParamId(j2534_0404::P2_MIN) | ComParamId(j2534_0404::P2_MAX)
        | PARAM_P2_STAR
        | PARAM_P2_STAR_ECU
        | PARAM_P2_MAX_ECU
        // CP_P3Func / CP_P3Phys (CAN context): stored service-level, no
        // J2534 SET_CONFIG equivalent for the CAN family (unlike KWP, where
        // these forward via native P3_MIN/P3_MAX -- see is_kwp_param and
        // service_params.rs). Pre-seeded by some CAN bustype defaults, so
        // they must be get/settable here too (previously unreachable).
        | PARAM_P3_FUNC
        | PARAM_P3_PHYS
        | PARAM_MODIFY_TIMING
        | PARAM_SESSION_TIMING_ECU
        | PARAM_SESSION_TIMING_OVERRIDE
        | PARAM_CAN_TRANSMISSION_TIME
        | PARAM_MESSAGE_INDICATION_RATE

        // ── Application: Error Handling ──────────────────────────────────────
        | PARAM_RC21_COMPLETION_TIMEOUT
        | PARAM_RC21_HANDLING
        | PARAM_RC21_REQUEST_TIME
        | PARAM_RC23_COMPLETION_TIMEOUT
        | PARAM_RC23_HANDLING
        | PARAM_RC23_REQUEST_TIME
        | PARAM_RC78_COMPLETION_TIMEOUT
        | PARAM_RC78_HANDLING
        | PARAM_RC_BYTE_OFFSET
        | PARAM_REPEAT_REQ_COUNT_APP
        | PARAM_SUSPEND_QUEUE_ON_ERROR

        // ── Application: COM ─────────────────────────────────────────────────
        // CP_ChangeSpeed* (0x8030-0x8033) and CP_ChangeSpeedTxDelay (0x8019):
        // accepted family-wide (ADR-164/Phase 4, SAE J2534-2 clause 9 SWCAN --
        // J2534-1 alone does not include SWCAN, but J2534-2 now does). All 5
        // are get/settable on every CAN-family CLL, not just an SW one --
        // `CP_ChangeSpeedRate`/`Ctrl`/`ResCtrl` translate to native
        // CONFIG_SW_CAN_* targets on an SW link specifically
        // (`comparam_id::to_j2534_config_id`); `CP_ChangeSpeedMsg`/
        // `CP_ChangeSpeedTxDelay` have no documented native mapping at all
        // (accepted-but-unmapped, mirroring `CP_P3Func`/`CP_P3Phys` above).
        | PARAM_CHANGE_SPEED_CTRL
        | PARAM_CHANGE_SPEED_MSG
        | PARAM_CHANGE_SPEED_RATE
        | PARAM_CHANGE_SPEED_RES_CTRL
        | PARAM_CHANGE_SPEED_TX_DELAY
        | PARAM_ENABLE_PERFORMANCE_TEST
        | PARAM_START_MSG_IND_ENABLE
        | PARAM_TRANSMIT_IND_ENABLE
        // CP_SwCan_HighVoltage: also accepted family-wide (ADR-164 Decision
        // 2) -- already allowlisted pre-Phase-4 (seeded in bus-type defaults
        // but never wired to a TxFlags bit until now, `rpc_primitive::
        // sw_can_tx_flags`).
        | PARAM_SW_CAN_HIGH_VOLTAGE

        // ── Physical layer: CAN FD (ADR-158/Phase 3a) ───────────────────────────
        // CP_CANFDBaudrate/CP_CANFDBitSamplePoint/CP_CANFDSyncJumpWidth have no
        // direct J2534-0404 SET_CONFIG equivalent, but CP_CANFDBaudrate (with
        // CP_CANFDTxMaxDataLength below) drives the connect-time FD-mode
        // substitution (`rpc_link::J2534Service::apply_fd_mode`) and the
        // FD_CAN_DATA_PHASE_RATE SET_CONFIG issued at connect time.
        | PARAM_CANFD_BAUDRATE
        | PARAM_CANFD_BIT_SAMPLE_POINT
        | PARAM_CANFD_SYNC_JUMP_WIDTH
        // CP_CANFDTxMaxDataLength (ADR-158): the other half of the FD-mode
        // trigger (`TX_DL > 8 || CP_CANFDBaudrate != 0`) -- previously seeded
        // in comparam_defaults.rs but absent from this allow-list, so
        // SetComParam rejected it outright.
        | PARAM_CANFD_TX_MAX_DATA_LENGTH
    )
}

// ── KWP family (ISO9141, ISO14230) ───────────────────────────────────────────

fn is_kwp_param(p: ComParamId) -> bool {
    matches!(
        p,
        // ── Physical layer (J2534 native) ─────────────────────────────────────
        // CP_UartConfig (S): encoded in DATA_BITS
        ComParamId(j2534_0404::DATA_BITS)
        // CP_Parity: J2534 specific; D-PDU encodes parity within CP_UartConfig
        | ComParamId(j2534_0404::PARITY)
        // 5-baud init mode
        | ComParamId(j2534_0404::FIVE_BAUD_MOD)
        // KWP timing (P-timers)
        | ComParamId(j2534_0404::P1_MIN) | ComParamId(j2534_0404::P1_MAX)
        | ComParamId(j2534_0404::P2_MIN) | ComParamId(j2534_0404::P2_MAX)
        | ComParamId(j2534_0404::P3_MIN) | ComParamId(j2534_0404::P3_MAX)
        | ComParamId(j2534_0404::P4_MIN) | ComParamId(j2534_0404::P4_MAX)
        // ISO9141 W-timers and bus-state timers
        | ComParamId(j2534_0404::W0)
        | ComParamId(j2534_0404::W1) | ComParamId(j2534_0404::W2) | ComParamId(j2534_0404::W3)
        | ComParamId(j2534_0404::W4) | ComParamId(j2534_0404::W5)
        | ComParamId(j2534_0404::TIDLE) | ComParamId(j2534_0404::TINIL) | ComParamId(j2534_0404::TWUP)
        // K-Line node address
        | ComParamId(j2534_0404::NODE_ADDRESS)

        // ── Physical layer (service-level) ────────────────────────────────────
        // CP_K_L_LineInit (O): stored, not forwarded
        | PARAM_K_L_LINE_INIT
        // CP_K_LinePullup (O): stored, not forwarded
        | PARAM_K_LINE_PULLUP
        // CP_W1Min/CP_W2Min/CP_W3Min/CP_W4Max (ADR-181): no native J2534
        // register for these sides (see the `is_kwp_param` doc comment's own
        // W-timer entry above, and `service_params.rs`'s doc comments on
        // each constant); stored, never forwarded.
        | PARAM_W1_MIN
        | PARAM_W2_MIN
        | PARAM_W3_MIN
        | PARAM_W4_MAX

        // ── Transport ─────────────────────────────────────────────────────────
        // CP_EcuRespSourceAddress, CP_FuncRespFormatPriorityType,
        // CP_FuncRespTargetAddr, CP_PhysRespFormatPriorityType, and
        // CP_MidRespId are PDU_PC_UNIQUE_ID class (see KWP_UNIQUE_ID_UNUM32
        // below) and are rejected by is_param_allowed before reaching this list.
        | PARAM_HEADER_FORMAT_KW
        | PARAM_REQUEST_ADDR_MODE
        | PARAM_FUNC_REQ_FORMAT_PRIORITY
        | PARAM_FUNC_REQ_TARGET_ADDR
        | PARAM_PHYS_REQ_FORMAT_PRIORITY
        | PARAM_PHYS_REQ_TARGET_ADDR
        // CP_EnableConcatenation (S): ISO 22900-2:2022 Table B.11 lists the
        // KWP family (ISO 9141-2, ISO 14230-2/-4) among the applicable
        // protocols.
        | PARAM_ENABLE_CONCATENATION
        | PARAM_FILLER_BYTE
        | PARAM_FILLER_BYTE_HANDLING
        | PARAM_FILLER_BYTE_LENGTH
        | PARAM_REPEAT_REQ_COUNT_TRANS
        | PARAM_5BAUD_ADDR_FUNC
        | PARAM_5BAUD_ADDR_PHYS
        | PARAM_INIT_SETTINGS
        | PARAM_MESSAGE_PRIORITY
        | PARAM_MID_REQ_ID

        // ── Application: Tester Present ──────────────────────────────────────
        | PARAM_TESTER_PRESENT_MSG
        | PARAM_TESTER_PRESENT_INTERVAL_US
        | PARAM_TESTER_PRESENT_ADDR_MODE
        | PARAM_TESTER_PRESENT_EXP_POS_RESP
        | PARAM_TESTER_PRESENT_EXP_NEG_RESP
        | PARAM_TESTER_PRESENT_HANDLING
        | PARAM_TESTER_PRESENT_REQ_RSP
        | PARAM_TESTER_PRESENT_SEND_TYPE
        | PARAM_TESTER_PRESENT_TIME_ECU

        // ── Application: Timing ──────────────────────────────────────────────
        | PARAM_CYCLIC_RESP_TIMEOUT
        | PARAM_P2_STAR
        | PARAM_P2_STAR_ECU
        | PARAM_P2_MAX_ECU
        | PARAM_MODIFY_TIMING
        | PARAM_SESSION_TIMING_ECU
        | PARAM_SESSION_TIMING_OVERRIDE
        | PARAM_MESSAGE_INDICATION_RATE

        // ── Application: Error Handling ──────────────────────────────────────
        | PARAM_RC21_COMPLETION_TIMEOUT
        | PARAM_RC21_HANDLING
        | PARAM_RC21_REQUEST_TIME
        | PARAM_RC23_COMPLETION_TIMEOUT
        | PARAM_RC23_HANDLING
        | PARAM_RC23_REQUEST_TIME
        | PARAM_RC78_COMPLETION_TIMEOUT
        | PARAM_RC78_HANDLING
        | PARAM_RC_BYTE_OFFSET
        | PARAM_REPEAT_REQ_COUNT_APP
        | PARAM_SUSPEND_QUEUE_ON_ERROR

        // ── Application: COM ─────────────────────────────────────────────────
        | PARAM_ENABLE_PERFORMANCE_TEST
        | PARAM_START_MSG_IND_ENABLE
        | PARAM_TRANSMIT_IND_ENABLE
    )
}

// ── J1850 PWM ────────────────────────────────────────────────────────────────

fn is_j1850pwm_param(p: ComParamId) -> bool {
    matches!(
        p,
        // CP_MidRespId is PDU_PC_UNIQUE_ID class (see J1850_UNIQUE_ID_UNUM32
        // below) and is rejected by is_param_allowed before reaching this list.
        // CP_NetworkLine (S, J1850PWM only): forwarded via NETWORK_LINE
        ComParamId(j2534_0404::NETWORK_LINE)
        | PARAM_HEADER_FORMAT_J1850
        | PARAM_REQUEST_ADDR_MODE
        // CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr/
        // CP_FuncReqFormatPriorityType/CP_FuncReqTargetAddr, and NODE_ADDRESS
        // (the J1850 tester source address): tx_header::j1850_header_bytes
        // reads all five to build the outgoing J1850 addressing header
        // (ADR-050/ADR-054), mirroring is_can_param's/is_kwp_param's
        // identical entries above -- previously missing here, so a J1850
        // client could never SetComParam an override and was stuck with
        // whatever a resource preset's own seeded defaults (or this
        // service's hardcoded fallback) supplied (backlog fix).
        | PARAM_PHYS_REQ_FORMAT_PRIORITY
        | PARAM_PHYS_REQ_TARGET_ADDR
        | PARAM_FUNC_REQ_FORMAT_PRIORITY
        | PARAM_FUNC_REQ_TARGET_ADDR
        | ComParamId(j2534_0404::NODE_ADDRESS)
        // CP_EnableConcatenation (S): ISO 22900-2:2022 Table B.11 lists
        // SAE J1850 VPW/PWM among the applicable protocols.
        | PARAM_ENABLE_CONCATENATION
        | PARAM_FILLER_BYTE
        | PARAM_FILLER_BYTE_HANDLING
        | PARAM_MID_REQ_ID
        | PARAM_MESSAGE_PRIORITY
        | PARAM_TESTER_PRESENT_MSG
        | PARAM_TESTER_PRESENT_INTERVAL_US
        | PARAM_TESTER_PRESENT_ADDR_MODE
        | PARAM_TESTER_PRESENT_HANDLING
        | PARAM_TESTER_PRESENT_REQ_RSP
        | PARAM_TESTER_PRESENT_SEND_TYPE
        | PARAM_CYCLIC_RESP_TIMEOUT
        | PARAM_MESSAGE_INDICATION_RATE
        | PARAM_RC21_COMPLETION_TIMEOUT
        | PARAM_RC21_HANDLING
        | PARAM_RC21_REQUEST_TIME
        | PARAM_RC23_COMPLETION_TIMEOUT
        | PARAM_RC23_HANDLING
        | PARAM_RC23_REQUEST_TIME
        | PARAM_RC78_COMPLETION_TIMEOUT
        | PARAM_RC78_HANDLING
        | PARAM_RC_BYTE_OFFSET
        | PARAM_REPEAT_REQ_COUNT_APP
        | PARAM_SUSPEND_QUEUE_ON_ERROR
        | PARAM_ENABLE_PERFORMANCE_TEST
        | PARAM_START_MSG_IND_ENABLE
        | PARAM_TRANSMIT_IND_ENABLE
        // CP_J1850IFRCtrl (O): J1850 In-Frame Response control, service-level only
        | PARAM_J1850_IFR_CTRL
    )
}

// ── J1850 VPW ────────────────────────────────────────────────────────────────

fn is_j1850vpw_param(p: ComParamId) -> bool {
    // Same as PWM but without NETWORK_LINE (PWM-only). CP_MidRespId is
    // PDU_PC_UNIQUE_ID class (see J1850_UNIQUE_ID_UNUM32 below) and is
    // rejected by is_param_allowed before reaching this list.
    matches!(
        p,
        PARAM_HEADER_FORMAT_J1850
        | PARAM_REQUEST_ADDR_MODE
        // CP_PhysReqFormatPriorityType/CP_PhysReqTargetAddr/
        // CP_FuncReqFormatPriorityType/CP_FuncReqTargetAddr, and NODE_ADDRESS
        // (the J1850 tester source address): tx_header::j1850_header_bytes
        // reads all five to build the outgoing J1850 addressing header
        // (ADR-050/ADR-054), mirroring is_can_param's/is_kwp_param's
        // identical entries -- previously missing here, so a J1850 client
        // could never SetComParam an override and was stuck with whatever a
        // resource preset's own seeded defaults (or this service's
        // hardcoded fallback) supplied (backlog fix).
        | PARAM_PHYS_REQ_FORMAT_PRIORITY
        | PARAM_PHYS_REQ_TARGET_ADDR
        | PARAM_FUNC_REQ_FORMAT_PRIORITY
        | PARAM_FUNC_REQ_TARGET_ADDR
        | ComParamId(j2534_0404::NODE_ADDRESS)
        // CP_EnableConcatenation (S): ISO 22900-2:2022 Table B.11 lists
        // SAE J1850 VPW/PWM among the applicable protocols.
        | PARAM_ENABLE_CONCATENATION
        | PARAM_FILLER_BYTE
        | PARAM_FILLER_BYTE_HANDLING
        | PARAM_MID_REQ_ID
        | PARAM_MESSAGE_PRIORITY
        | PARAM_TESTER_PRESENT_MSG
        | PARAM_TESTER_PRESENT_INTERVAL_US
        | PARAM_TESTER_PRESENT_ADDR_MODE
        | PARAM_TESTER_PRESENT_HANDLING
        | PARAM_TESTER_PRESENT_REQ_RSP
        | PARAM_TESTER_PRESENT_SEND_TYPE
        | PARAM_CYCLIC_RESP_TIMEOUT
        | PARAM_MESSAGE_INDICATION_RATE
        | PARAM_RC21_COMPLETION_TIMEOUT
        | PARAM_RC21_HANDLING
        | PARAM_RC21_REQUEST_TIME
        | PARAM_RC23_COMPLETION_TIMEOUT
        | PARAM_RC23_HANDLING
        | PARAM_RC23_REQUEST_TIME
        | PARAM_RC78_COMPLETION_TIMEOUT
        | PARAM_RC78_HANDLING
        | PARAM_RC_BYTE_OFFSET
        | PARAM_REPEAT_REQ_COUNT_APP
        | PARAM_SUSPEND_QUEUE_ON_ERROR
        | PARAM_ENABLE_PERFORMANCE_TEST
        | PARAM_START_MSG_IND_ENABLE
        | PARAM_TRANSMIT_IND_ENABLE
        // CP_J1850IFRCtrl (O): J1850 In-Frame Response control, service-level only
        | PARAM_J1850_IFR_CTRL
    )
}

// ── SCI family (SCI-A/B Engine/Trans, SAE J2610) ─────────────────────────────

fn is_sci_param(p: ComParamId) -> bool {
    matches!(
        p,
        // CP_UartConfig (S): accepted and range-checked (ADR-071), but SCI
        // has no J2534 SET_CONFIG support (to_j2534_config_id returns None
        // for SCI protocols) -- stored in the Working ComParam set only,
        // never forwarded to hardware.
        ComParamId(j2534_0404::DATA_BITS)
        // SCI timing
        | ComParamId(j2534_0404::T1_MAX)
        | ComParamId(j2534_0404::T2_MAX)
        | ComParamId(j2534_0404::T3_MAX)
        | ComParamId(j2534_0404::T4_MAX)
        | ComParamId(j2534_0404::T5_MAX)
        // Service params
        | PARAM_SCI_TRANSMIT_MODE
        | PARAM_SCI_SET_PROG_VOLTAGE
        | PARAM_TESTER_PRESENT_MSG
        | PARAM_TESTER_PRESENT_INTERVAL_US
        | PARAM_CYCLIC_RESP_TIMEOUT
        | PARAM_MESSAGE_INDICATION_RATE
        | PARAM_ENABLE_PERFORMANCE_TEST
        | PARAM_START_MSG_IND_ENABLE
        | PARAM_TRANSMIT_IND_ENABLE
    )
}

// ── UNIQUE_ID class param sets ────────────────────────────────────────────────

/// CAN / ISO 15765 unum32 params with PDU_PC_UNIQUE_ID class.
///
/// These params identify per-ECU addressing on CAN-based protocols and are
/// managed through the UniqueRespIdTable in addition to the regular ComParam set.
const CAN_UNIQUE_ID_UNUM32: &[ComParamId] = &[
    PARAM_CAN_PHYS_REQ_EXT_ADDR,     // CP_CanPhysReqExtAddr
    PARAM_CAN_PHYS_REQ_FORMAT,       // CP_CanPhysReqFormat
    PARAM_CAN_PHYS_REQ_ID,           // CP_CanPhysReqId
    PARAM_CAN_RESP_USDT_EXT_ADDR,    // CP_CanRespUSDTExtAddr
    PARAM_CAN_RESP_USDT_FORMAT,      // CP_CanRespUSDTFormat
    PARAM_CAN_RESP_USDT_ID,          // CP_CanRespUSDTId
    PARAM_CAN_RESP_UUDT_EXT_ADDR,    // CP_CanRespUUDTExtAddr
    PARAM_CAN_RESP_UUDT_FORMAT,      // CP_CanRespUUDTFormat
    PARAM_CAN_RESP_UUDT_ID,          // CP_CanRespUUDTId
    PARAM_ECU_RESP_SOURCE_ADDR,      // CP_EcuRespSourceAddress
    PARAM_FUNC_RESP_FORMAT_PRIORITY, // CP_FuncRespFormatPriorityType
    PARAM_FUNC_RESP_TARGET_ADDR,     // CP_FuncRespTargetAddr
    PARAM_PHYS_RESP_FORMAT_PRIORITY, // CP_PhysRespFormatPriorityType
    PARAM_MID_RESP_ID,               // CP_MidRespId
                                     // `PARAM_J1939_SOURCE_ADDRESS`/`PARAM_J1939_SOURCE_NAME` used to be
                                     // (mis)listed here (ADR-184): `J1939_PS.is_can_family() == false`, so
                                     // neither was ever actually reachable through this CAN-family list --
                                     // `unique_id_params` only ever dispatches here for `CAN`/`ISO15765`.
                                     // `CP_J1939SourceAddress` now has its own `J1939_UNIQUE_ID_UNUM32` list
                                     // below, reached via `unique_id_params`'s own `J1939_PS` branch;
                                     // `CP_J1939SourceName` matching is an accepted, documented residual
                                     // (the backlog) and is not
                                     // PDU_PC_UNIQUE_ID class for any protocol as of this change -- it stays
                                     // a plain settable ComParam via `is_j1939_param`.
];

/// CAN / ISO 15765 bytefield params with PDU_PC_UNIQUE_ID class.
///
/// Empty (ADR-184): `PARAM_J1939_SOURCE_NAME` was this list's sole,
/// unreachable-for-CAN entry (see `CAN_UNIQUE_ID_UNUM32`'s own doc comment)
/// and has been removed rather than relocated -- NAME-based RX matching
/// remains out of scope this pass.
const CAN_UNIQUE_ID_BYTES: &[ComParamId] = &[];

/// SAE J1939 (`J1939_PS`) unum32 params with `PDU_PC_UNIQUE_ID` class
/// (ADR-184). `CP_J1939SourceAddress` identifies a responding ECU's SAE
/// J1939 source address (clause 16.4.3's identifier low byte) and is
/// managed exclusively through the UniqueRespIdTable, like every other
/// `PDU_PC_UNIQUE_ID`-class param -- ISO 22900-2 §9.3.3.6. No bytefield
/// counterpart: `CP_J1939SourceName` deliberately stays OUT of this list
/// (see `CAN_UNIQUE_ID_BYTES`'s own doc comment) -- it remains a plain
/// settable ComParam via `is_j1939_param` instead.
const J1939_UNIQUE_ID_UNUM32: &[ComParamId] = &[PARAM_J1939_SOURCE_ADDRESS];

/// KWP (ISO 9141 / ISO 14230) unum32 params with PDU_PC_UNIQUE_ID class.
const KWP_UNIQUE_ID_UNUM32: &[ComParamId] = &[
    PARAM_ECU_RESP_SOURCE_ADDR,      // CP_EcuRespSourceAddress
    PARAM_FUNC_RESP_FORMAT_PRIORITY, // CP_FuncRespFormatPriorityType
    PARAM_FUNC_RESP_TARGET_ADDR,     // CP_FuncRespTargetAddr
    PARAM_PHYS_RESP_FORMAT_PRIORITY, // CP_PhysRespFormatPriorityType
    PARAM_MID_RESP_ID,               // CP_MidRespId
];

/// J1850 PWM / VPW unum32 params with PDU_PC_UNIQUE_ID class (ADR-202).
///
/// ISO 22900-2:2022 Table B.11 marks `CP_EcuRespSourceAddress`,
/// `CP_FuncRespFormatPriorityType`, `CP_FuncRespTargetAddr`, and
/// `CP_PhysRespFormatPriorityType` as mandatory-support `PDU_PC_UNIQUE_ID`-class
/// for SAE J1850 VPW/PWM -- the same 4 params `KWP_UNIQUE_ID_UNUM32` already
/// lists for the KWP family. This list previously held only `CP_MidRespId`,
/// which left `SetUniqueRespIdTable` rejecting every J1850 attempt to
/// configure per-ECU response addressing even though `tx_header.rs`'s
/// `ecu_addr`/`response_header_bytes` already reads `CP_EcuRespSourceAddress`
/// from the table identically for KWP and J1850.
///
/// `CP_MidRespId` is retained here even though Table B.11 actually scopes it
/// to SAE J1708 only, not J1850 -- Table B.11 is a minimum-support matrix (an
/// empty cell means "not required", not "forbidden"), and this repo already
/// carries the same documented surplus entry for `CAN_UNIQUE_ID_UNUM32` and
/// `KWP_UNIQUE_ID_UNUM32` (see the companion P3 backlog entry in
/// `docs/implementation-notes.md`, extended by ADR-202 to also cover this
/// list). A future uniform cleanup pass, if ever done, removes it from all
/// three lists together.
const J1850_UNIQUE_ID_UNUM32: &[ComParamId] = &[
    PARAM_ECU_RESP_SOURCE_ADDR,      // CP_EcuRespSourceAddress
    PARAM_FUNC_RESP_FORMAT_PRIORITY, // CP_FuncRespFormatPriorityType
    PARAM_FUNC_RESP_TARGET_ADDR,     // CP_FuncRespTargetAddr
    PARAM_PHYS_RESP_FORMAT_PRIORITY, // CP_PhysRespFormatPriorityType
    PARAM_MID_RESP_ID,               // CP_MidRespId (surplus/inert, not incorrect; see above)
];

/// Returns the lists of unum32 and bytefield param IDs that have
/// `PDU_PC_UNIQUE_ID` class for `protocol`.
///
/// Used by `GetUniqueRespIdTable` to build the template entry returned when no
/// table has been configured yet (ISO 22900-2 §9.3.3.6).  The tuple is
/// `(unum32_param_ids, bytefield_param_ids)`.
pub(super) fn unique_id_params(
    protocol: ChannelProtocol,
) -> (&'static [ComParamId], &'static [ComParamId]) {
    if protocol.is_can_family() {
        (CAN_UNIQUE_ID_UNUM32, CAN_UNIQUE_ID_BYTES)
    } else if protocol.is_kwp_family() {
        (KWP_UNIQUE_ID_UNUM32, &[])
    } else if matches!(
        protocol.j2534_protocol_id(),
        j2534_0404::J1850PWM | j2534_0404::J1850VPW
    ) {
        (J1850_UNIQUE_ID_UNUM32, &[])
    } else if resources::is_j1939_protocol_id(protocol.j2534_protocol_id()) {
        // ADR-184: mirrors `is_param_allowed`'s own `resources::
        // is_j1939_protocol_id` dispatch (not exact `J1939_PS` identity) so
        // the `_CH1..128` range is covered too, consistent with that
        // function's own reasoning even though no resource-table row or
        // connect path reaches an Additional Channel yet.
        (J1939_UNIQUE_ID_UNUM32, &[])
    } else {
        // SCI and unknown protocols: no UNIQUE_ID params
        (&[], &[])
    }
}

/// Returns `true` if `param_id` has `PDU_PC_UNIQUE_ID` class for `protocol`.
///
/// Used by `is_param_allowed` to reject `GetComParam` / `SetComParam` for
/// params that ISO 22900-2 §9.3.3.6 reserves exclusively for
/// `GetUniqueRespIdTable` / `SetUniqueRespIdTable`.
fn is_unique_id_param(protocol: ChannelProtocol, param_id: ComParamId) -> bool {
    let (unum32, bytes) = unique_id_params(protocol);
    unum32.contains(&param_id) || bytes.contains(&param_id)
}

// ── BUSTYPE class param sets (ADR-067) ───────────────────────────────────────
//
// `PDU_PC_BUSTYPE`-class ComParams identify the physical bus/channel
// configuration itself (baud rate, CAN bit timing/sample point, UART config,
// termination, network line selection) rather than any one ComPrimitive's
// request/response transaction. ISO 22900-2 §9.4.3 does not allow
// `temp_param_update` to stage a *different* value for a BUSTYPE-class
// ComParam: `rpc_start_com_primitive` rejects such a call with
// `PDU_ERR_TEMPPARAM_NOT_ALLOWED` before it has any other effect (no
// enqueue, no Working/Active writeback -- see `bustype_params_differ`).
//
// This list is drawn from `comparam-protocol-support.md`'s "Physical Layer
// ComParams (BUSTYPE class)" table, which already documents the ISO 22900-3
// class assignment for every physical-layer ComParam this service supports.
//
// This list is keyed by *physical hardware effect*, not by ISO `PDU_PC_BUSTYPE`
// label membership: every entry here is a ComParam whose value reaches a
// native `PassThruIoctl SET_CONFIG` write (directly, or via one of the two
// `apply_bustype_lock`/`strip_bustype_keys` consumers below) that another
// CLL's physical-ComParam lock or a `temp_param_update` bracket must never
// touch. `CP_Parity` (`ComParamId(j2534_0404::PARITY)`) is included on that
// basis (ADR-110 amendment, fixing a Codex-review finding on PR #116): an
// earlier draft of this list excluded it as "conservatively ambiguous"
// because it has no distinct D-PDU `CP_*` name of its own (D-PDU folds
// parity into `CP_UartConfig`, mapped here to `DATA_BITS` alone). That
// exclusion was correct for `bustype_params_differ`'s *original*, narrower
// purpose (deciding whether a `PDU_PC_BUSTYPE`-labeled ISO ComParam
// genuinely changed), but this same list is now also reused by
// `apply_bustype_lock`'s `hw_set` exclusion and `strip_bustype_keys` for a
// different purpose entirely: "which keys must never reach physical
// hardware while locked / via a temp bracket." `expand_uart_config`'s
// explicit-`PARITY`-wins precedence shows `CP_Parity`'s physical effect is
// completely unambiguous -- an explicit `PARITY` entry always reaches
// hardware, unconditionally, and wins over any `CP_UartConfig`-derived
// parity value -- so excluding it from *this* list left a hole a
// non-owning CLL (or a `temp_param_update` call) could use to smuggle a
// physical UART-parity change past both guards. One list, keyed by hardware
// effect, closes that hole for both consumers at once; see ADR-110's
// amendment section and ADR-067 §E's amendment note for the full rationale,
// including the side effect that ADR-067 §E's own `bustype_params_differ`
// guard now also rejects a `temp_param_update` call staging a `CP_Parity`
// difference from Active (spec-correct per ISO 22900-2 §9.4.16.2.1 c)
// NOTE 2, not a regression).
//
// `NODE_ADDRESS` (`ComParamId(j2534_0404::NODE_ADDRESS)`, D-PDU name
// `CP_Node_Address`) joined this list on the same hardware-effect basis as
// `CP_Parity` above (PR #53 review round, Codex-review-confirmed finding):
// `ComParamId::to_j2534_config_id` (`comparam_id.rs`) translates it to a real
// native `SET_CONFIG` write whenever the connected protocol is `J1850PWM`,
// and PR #53 made it client-writable via `SetComParam` for BOTH J1850PWM and
// J1850VPW (`is_j1850pwm_param`/`is_j1850vpw_param`) for the first time.
// Before this addition, a non-owning CLL sharing the same physical channel
// could stage a `NODE_ADDRESS` difference from Active and have
// `apply_bustype_lock` push it to hardware unfiltered while another CLL held
// `LOCK_PHYSICAL_COM_PARAMS` -- the same hole `CP_Parity`'s addition closed
// -- and `strip_bustype_keys` would not strip it from a `temp_param_update`
// bracket either, letting a temp update stage a physical `NODE_ADDRESS`
// change ISO 22900-2 §9.4.16.2.1 c)/d) does not allow. `NODE_ADDRESS` is
// allowed on both J1850PWM and J1850VPW even though it only has a native
// hardware translation on J1850PWM specifically -- this mirrors `CP_Parity`
// (also in this list, also protocol-conditionally translated) exactly, and
// is not a reason to exclude it here.

/// Physical-layer (`PDU_PC_BUSTYPE`) unum32 ComParams this service supports.
const BUSTYPE_UNUM32: &[ComParamId] = &[
    ComParamId(j2534_0404::DATA_RATE),        // CP_Baudrate
    ComParamId(j2534_0404::BIT_SAMPLE_POINT), // CP_BitSamplePoint
    PARAM_BIT_SAMPLE_POINT_ECU,               // CP_BitSamplePoint_Ecu
    PARAM_SAMPLES_PER_BIT,                    // CP_SamplesPerBit
    PARAM_SAMPLES_PER_BIT_ECU,                // CP_SamplesPerBit_Ecu
    ComParamId(j2534_0404::SYNC_JUMP_WIDTH),  // CP_SyncJumpWidth
    PARAM_SYNC_JUMP_WIDTH_ECU,                // CP_SyncJumpWidth_Ecu
    PARAM_LISTEN_ONLY,                        // CP_ListenOnly
    PARAM_TERMINATION_TYPE,                   // CP_TerminationType
    PARAM_TERMINATION_TYPE_ECU,               // CP_TerminationType_Ecu
    ComParamId(j2534_0404::NETWORK_LINE),     // CP_NetworkLine
    PARAM_K_L_LINE_INIT,                      // CP_K_L_LineInit
    PARAM_K_LINE_PULLUP,                      // CP_K_LinePullup
    ComParamId(j2534_0404::DATA_BITS),        // CP_UartConfig (mapped to DATA_BITS; see note above)
    ComParamId(j2534_0404::PARITY),           // CP_Parity (ADR-110 amendment; see note above)
    PARAM_CANFD_BAUDRATE,                     // CP_CANFDBaudrate
    PARAM_CANFD_BIT_SAMPLE_POINT,             // CP_CANFDBitSamplePoint
    PARAM_CANFD_SYNC_JUMP_WIDTH,              // CP_CANFDSyncJumpWidth
    PARAM_J1850_IFR_CTRL,                     // CP_J1850IFRCtrl
    ComParamId(j2534_0404::NODE_ADDRESS), // CP_Node_Address (PR #53 review round; see note above)
    // SAE J2534-2 clause 10 Analog Inputs (ADR-178, Codex review PR #70 round 2): connect-time-
    // latched via SET_CONFIG(CONFIG_SAMPLE_RATE), the same physical-layer shape
    // PARAM_CANFD_BAUDRATE/PARAM_CANFD_BIT_SAMPLE_POINT/PARAM_CANFD_SYNC_JUMP_WIDTH already have
    // above despite also having no native SET_CONFIG-forwarded ComParam ID of their own -- without
    // this entry, GetComParam misreported PduPcSpecified instead of PduPcBustype for this param,
    // and bustype_params_differ (the CoptTempparamUpdate rejection guard) could not see a staged
    // rate differing from Active, letting a temp-param update silently stage a different
    // connect-latched rate the CoptUpdateparam guard (rpc_primitive.rs) exists specifically to
    // reject.
    PARAM_ANALOG_SAMPLE_RATE, // CP_AnalogSampleRate
    // SAE J2534-2 clause 10 Analog Inputs (ADR-216 Decision item 7): the
    // four writable Analog Inputs ComParams are channel-wide, not per-CLL,
    // the same rationale already established for CP_AnalogSampleRate just
    // above -- a CoptUpdateparam temp-update attempt must be refused the
    // same way (bustype_params_differ/the physical-ComParam lock), and
    // strip_bustype_keys keeps them correctly excluded from an unrelated
    // CLL's own bracket-revert. The ten UEB timing ComParams and the three
    // read-only Analog Inputs ComParams stay OUT of this list deliberately
    // (default, per-CLL classification) -- see ADR-216 Decision item 7.
    PARAM_ANALOG_ACTIVE_CHANNELS,     // CP_AnalogActiveChannels
    PARAM_ANALOG_SAMPLES_PER_READING, // CP_AnalogSamplesPerReading
    PARAM_ANALOG_READINGS_PER_MSG,    // CP_AnalogReadingsPerMsg
    PARAM_ANALOG_AVERAGING_METHOD,    // CP_AnalogAveragingMethod
];

/// Physical-layer (`PDU_PC_BUSTYPE`) bytefield ComParams this service supports.
const BUSTYPE_BYTES: &[ComParamId] = &[
    PARAM_CAN_BAUDRATE_RECORD, // CP_CanBaudrateRecord
];

/// Channel-wide (per-physical-channel), hardware-resident ComParams that are
/// deliberately NOT `PDU_PC_BUSTYPE`-classified -- the same classification
/// basis as `CP_Cs`/`CONFIG_J1939_BRDCST_MIN_DELAY` and
/// `CP_TP20BroadcastInterval`/`CONFIG_TP2_0_T_BR_INT` (ADR-192 Decision item
/// 3): pacing-class, not bus-configuration-class, so it never belongs in
/// `BUSTYPE_UNUM32`/`BUSTYPE_BYTES`. That deliberate exclusion means
/// `strip_bustype_keys` alone does not keep one of these keys out of a
/// per-CLL `Active` push during a `temp_param_update` bracket's revert --
/// and `Active` is tracked per-CLL, not per-channel, so a bracket reverting
/// to ITS OWN Active for one of these keys can clobber the channel's real,
/// currently-live value if a sibling CLL sharing the same physical channel
/// has since promoted a different value of its own (round 18, Codex review,
/// P2, PR #101, ADR-192 Decision item 3 amendment). `strip_captured_channel_
/// wide_keys` (below) is this const's companion function, closing exactly
/// that gap when a captured restore pair actually exists --
/// see `events.rs`'s `capture_channel_wide_hardware_locked`/
/// `apply_params_to_hardware_capturing` for the actual fix (a captured
/// pre-bracket HARDWARE value, read while `api` is held for the bracket, is
/// restored on revert instead of this CLL's own per-CLL Active).
pub(super) const CHANNEL_WIDE_UNUM32: &[ComParamId] = &[PARAM_TP20_BROADCAST_INTERVAL];

/// Returns `true` if `param_id` is a `PDU_PC_BUSTYPE`-class Bytefield param
/// (currently only `CP_CanBaudrateRecord`).
///
/// Used by `rpc_get_com_param` to report the correct `ParamData` oneof
/// variant (an empty Bytefield, not `Unum32(0)`) for a bus type that leaves
/// this param without a default -- e.g. `ISO_11898_3_DWFTCAN`/
/// `SAE_J2411_SWCAN`, for which ISO 22900-2:2009(E) Table B.21 defines no
/// `CP_CanBaudrateRecord` default at all (ADR-130).
pub(super) fn is_bustype_bytes_param(param_id: ComParamId) -> bool {
    BUSTYPE_BYTES.contains(&param_id)
}

/// Returns `true` if `param_id` is `PDU_PC_BUSTYPE` class (the union of
/// `BUSTYPE_UNUM32`/`BUSTYPE_BYTES`).
///
/// This reuses the same hardware-effect-keyed list `apply_bustype_lock`/
/// `strip_bustype_keys` use, not a separate ISO-label-keyed list -- safe by
/// a "class report follows enforcement" invariant (ADR-133), not by
/// coincidence: every member of this list *receives* full BUSTYPE
/// enforcement (locked-out under another CLL's physical lock, rejected from
/// `temp_param_update`) by construction, so reporting `PDU_PC_BUSTYPE` for
/// it is always behavior-accurate regardless of whether the member also
/// happens to be a distinct, independently-named ISO ComParam. `CP_Parity`
/// (`ComParamId(j2534_0404::PARITY)`) is the concrete case this matters
/// for: per the doc comment above `BUSTYPE_UNUM32`, it has no distinct
/// D-PDU `CP_*` name of its own (folded into `CP_UartConfig`/`DATA_BITS`)
/// and was added on a hardware-effect basis, not an ISO-label basis -- yet
/// it still reports `PDU_PC_BUSTYPE` here correctly, because it still gets
/// the full BUSTYPE lock/temp-update treatment those functions enforce; a
/// class report of `PDU_PC_SPECIFIED` ("no class restrictions known") would
/// be the actual bug, since a client relying on it would then hit
/// `PDU_ERR_EVT_RSC_LOCKED`/`PDU_ERR_TEMPPARAM_NOT_ALLOWED` anyway. If a
/// future member is ever added to `BUSTYPE_UNUM32`/`BUSTYPE_BYTES` whose
/// intended class handling is *not* BUSTYPE, the enforcement itself would
/// be the bug to fix, not this reporting function.
pub(super) fn is_bustype_param(param_id: ComParamId) -> bool {
    BUSTYPE_UNUM32.contains(&param_id) || BUSTYPE_BYTES.contains(&param_id)
}

/// `CP_TesterPresentxxx` params -- `PDU_PC_TESTER_PRESENT` class per ISO
/// 22900-2 Table B.6 (name-scoped: the class covers the tester-present
/// ComParams named CP_TesterPresentxxx; ISO 22900-2 also explicitly permits
/// supplier-specific ComParams, spec lines 5328/5557, so this classification
/// is not limited to the nine standardized members). Includes
/// `PARAM_TESTER_PRESENT_IMMED` (`CP_TesterPresentImmed`, 0x80B1): its
/// semantics are purely tester-present handling (toggling immediate-send-at-
/// `CoptStartcomm` behavior), matching the same naming/function rule, even
/// though it has no dedicated protocol-allowlist entry of its own today and
/// is a supplier-specific extension not present in the ISO 22900-2:2009(E)
/// text (ADR-133). See the backlog for a separate, pre-existing, unrelated bug this param has: it is
/// seeded into ~10 `comparam_defaults.rs` presets and documented as
/// supported in `comparam-protocol-support.md`, but is absent from every
/// `is_*_param` allowlist below, so it is unreachable via `GetComParam`/
/// `SetComParam` on any real protocol today (only via the unknown-protocol
/// allow-all fallback) -- this classification is correct either way that
/// backlog item resolves, since removing the param would remove this list
/// entry too, and wiring it up would not change its class.
const TESTER_PRESENT_CLASS_PARAMS: &[ComParamId] = &[
    PARAM_TESTER_PRESENT_MSG,
    PARAM_TESTER_PRESENT_INTERVAL_US,
    PARAM_TESTER_PRESENT_ADDR_MODE,
    PARAM_TESTER_PRESENT_EXP_POS_RESP,
    PARAM_TESTER_PRESENT_EXP_NEG_RESP,
    PARAM_TESTER_PRESENT_HANDLING,
    PARAM_TESTER_PRESENT_REQ_RSP,
    PARAM_TESTER_PRESENT_SEND_TYPE,
    PARAM_TESTER_PRESENT_TIME_ECU,
    PARAM_TESTER_PRESENT_IMMED,
];

/// Returns the `PDU_PC_*` class (ISO 22900-2 Annex B.3.2) that `GetComParam`
/// should report for `param_id`.
///
/// Only `PDU_PC_BUSTYPE` and `PDU_PC_TESTER_PRESENT` are reported -- the two
/// classes with D-PDU-API-visible behavioral consequences (lock/TempParamUpdate
/// rules) that this service can determine from a verified spec citation.
/// `PDU_PC_UNIQUE_ID` is never reachable here: `is_unique_id_param` rejects
/// those params in `check_param_allowed` before a class is ever assigned
/// (ISO 22900-2 §9.3.3.6, ADR-042). Every other param reports
/// `PDU_PC_SPECIFIED` (0) -- not a real ISO 22900-2 class value (the real
/// `E_PDU_PC` enum only defines 1-7), but the proto's required zero value,
/// meaning "unclassified/not reported": the only in-workspace source that
/// could classify the rest (ISO 22900-2:2009(E) Tables B.10/B.11's
/// PARAM-CLASS column) is flagged unreliable by the document's own
/// conversion note and is internally inconsistent where spot-checked, so no
/// full TIMING/INIT/COM/ERRHDL mapping can currently be verified to this
/// audit's standard. See ADR-133.
pub(super) fn com_param_class(param_id: ComParamId) -> vci_service_interface::PduParamClass {
    if is_bustype_param(param_id) {
        vci_service_interface::PduParamClass::PduPcBustype
    } else if TESTER_PRESENT_CLASS_PARAMS.contains(&param_id) {
        vci_service_interface::PduParamClass::PduPcTesterPresent
    } else {
        vci_service_interface::PduParamClass::PduPcSpecified
    }
}

/// Every Bytefield-typed ComParam this service supports via `GetComParam`/
/// `SetComParam` -- exhaustive by construction: `rpc_set_com_param`'s
/// Bytefield match arm rejects any other `param_id` with `unimplemented`.
pub(super) const BYTEFIELD_PARAMS: &[ComParamId] = &[
    PARAM_TESTER_PRESENT_MSG,
    PARAM_TESTER_PRESENT_EXP_POS_RESP,
    PARAM_TESTER_PRESENT_EXP_NEG_RESP,
    PARAM_J1939_PREFERRED_ADDRESS,
    PARAM_J1939_NAME,
    PARAM_J1939_NAME_ECU,
    PARAM_J1939_TARGET_NAME,
    PARAM_CAN_BAUDRATE_RECORD,
    PARAM_CHANGE_SPEED_MSG,
];

/// Every Structfield-typed ComParam this service supports -- exhaustive by
/// construction, same rationale as `BYTEFIELD_PARAMS`.
pub(super) const STRUCTFIELD_PARAMS: &[ComParamId] = &[
    PARAM_EXTENDED_TIMING,
    PARAM_SESSION_TIMING_OVERRIDE,
    PARAM_ACCESS_TIMING_ECU,
    PARAM_ACCESS_TIMING_OVERRIDE,
    PARAM_SESSION_TIMING_ECU,
];

/// Validates a client-supplied `ParamStructfield` for `SetComParam(param_id,
/// ...)` before it is stored (Codex review, PR #17, two findings on the
/// same input path):
///
/// 1. The oneof `data` variant must match what `param_id` actually is --
///    `STRUCTFIELD_PARAMS` lumps `PDU_CPST_ACCESS_TIMING`-shaped params
///    (`CP_ExtendedTiming`/`CP_AccessTiming_Ecu`/`CP_AccessTimingOverride`)
///    together with `PDU_CPST_SESSION_TIMING`-shaped ones
///    (`CP_SessionTimingOverride`/`CP_SessionTiming_Ecu`) under one
///    `is_structfield_param` check with no shape check at all -- without
///    this, a client could `SetComParam` a `SessionTiming`-shaped value
///    onto e.g. `CP_AccessTiming_Ecu`, and a later qualifying ADR-146
///    Access Timing response would hit `store_access_timing_ecu_entry`'s
///    `unreachable!()` on that wrong variant, panicking the channel's poll
///    task. `data: None` (an explicitly empty Structfield) is valid for
///    every member -- there is nothing to mismatch.
/// 2. For an `AccessTiming`-shaped value specifically, every entry's 5
///    timing fields and `timing_set` are proto `u32` but represent ISO
///    14230-2 wire bytes (`UNUM8`, ADR-146) -- without a range check, a
///    value above 255 silently truncates via `as u8` wherever this
///    ComParam is later read (`access_timing_structfield_entry`), which
///    could feed drastically different timing than the client actually
///    supplied into a later TPI=1 reapply or TPI=2 override redirection.
pub(super) fn validate_structfield_shape(
    param_id: ComParamId,
    sf: &vci_service_interface::ParamStructfield,
) -> Result<(), String> {
    use vci_service_interface::param_structfield::Data;
    match sf.data.as_ref() {
        None => Ok(()),
        Some(Data::AccessTiming(list)) => {
            if !matches!(
                param_id,
                PARAM_EXTENDED_TIMING | PARAM_ACCESS_TIMING_ECU | PARAM_ACCESS_TIMING_OVERRIDE
            ) {
                return Err(format!(
                    "ComParam {:#010x} does not accept an AccessTiming Structfield",
                    param_id.0
                ));
            }
            for e in &list.entries {
                for (name, value) in [
                    ("p2_min", e.p2_min),
                    ("p2_max", e.p2_max),
                    ("p3_min", e.p3_min),
                    ("p3_max", e.p3_max),
                    ("p4_min", e.p4_min),
                    ("timing_set", e.timing_set),
                ] {
                    if value > u8::MAX as u32 {
                        return Err(format!(
                            "AccessTiming field {name}={value} exceeds the ISO 14230-2 wire \
                             byte range (0-255) for ComParam {:#010x}",
                            param_id.0
                        ));
                    }
                }
            }
            Ok(())
        }
        Some(Data::SessionTiming(list)) => {
            if !matches!(
                param_id,
                PARAM_SESSION_TIMING_OVERRIDE | PARAM_SESSION_TIMING_ECU
            ) {
                return Err(format!(
                    "ComParam {:#010x} does not accept a SessionTiming Structfield",
                    param_id.0
                ));
            }
            // Codex review, PR #22 round 1: `session`/`p2_max`/`p2_star` are
            // `u32` proto fields but ADR-150's consumers
            // (`session_timing_structfield_entries`, `store_session_timing_ecu_entry`)
            // narrow them to `u16` with `as`, which silently wraps rather
            // than rejects -- an override of `p2_max=65536` would truncate
            // to a P2Max of 0 instead of being rejected. Rejecting here, at
            // the SAME `rpc_set_com_param` validation point `AccessTiming`'s
            // own field-range check above already uses, closes it before
            // either consumer ever sees the value.
            for e in &list.entries {
                for (name, value) in [("p2_max", e.p2_max), ("p2_star", e.p2_star)] {
                    if value > u16::MAX as u32 {
                        return Err(format!(
                            "SessionTiming field {name}={value} exceeds the u16 wire range \
                             (0-65535) for ComParam {:#010x}",
                            param_id.0
                        ));
                    }
                }
                // Codex review, PR #22 round 2: `session` is narrower than a
                // bare `u16` -- `SessionTimingConfig::with_request` only
                // ever captures `1..=127` (the request subFunction's low 7
                // bits, masked with `& 0x7F`, with 0 filtered out --
                // ISO 22900-2 §B.3.3.2.2's documented valid range). A
                // client-supplied `session` outside `1..=127` (0, or
                // 128-65535, all in-range for the `u16` check above but
                // never producible by `with_request`) can therefore never
                // match any captured request and is silently, permanently
                // dead input -- reject it explicitly instead.
                if e.session == 0 || e.session > 127 {
                    return Err(format!(
                        "SessionTiming field session={} is outside the valid range 1-127 \
                         (ISO 22900-2 section B.3.3.2.2) for ComParam {:#010x}",
                        e.session, param_id.0
                    ));
                }
            }
            Ok(())
        }
        Some(Data::VendorSpecific(_)) | Some(Data::TlsVersionAndCipher(_)) => Err(format!(
            "ComParam {:#010x} does not accept this Structfield variant",
            param_id.0
        )),
    }
}

pub(super) fn is_bytefield_param(param_id: ComParamId) -> bool {
    BYTEFIELD_PARAMS.contains(&param_id)
}

pub(super) fn is_structfield_param(param_id: ComParamId) -> bool {
    STRUCTFIELD_PARAMS.contains(&param_id)
}

/// Effective (`GetComParam`-observable) value of a Unum32 ComParam in
/// `set`: an explicit entry if present, else the ADR-071
/// `CP_UartConfig`-derived parity for `PARITY` specifically, else `0`.
/// MUST mirror `rpc_get_com_param`'s own Unum32 fallback exactly -- that
/// function now calls this helper instead of duplicating the logic, so
/// there is one source of truth (a presence-vs-effective-value mismatch
/// between the two copies is exactly the bug this function exists to
/// close -- see ADR-133's amendment for the incident this fixes).
pub(super) fn effective_unum32(set: &ComParamSet, id: ComParamId) -> u32 {
    set.unum32.get(&id).copied().unwrap_or_else(|| {
        if id == ComParamId(j2534_0404::PARITY) {
            set.unum32
                .get(&ComParamId(j2534_0404::DATA_BITS))
                .copied()
                .and_then(uart_config_to_parity)
                .unwrap_or(0)
        } else {
            0
        }
    })
}

/// Effective (`GetComParam`-observable) value of a Bytefield ComParam in
/// `set`: an explicit entry if present, else empty -- matches
/// `rpc_get_com_param`'s `is_bytefield_param` fallback (A2-18, ADR-133).
pub(super) fn effective_bytes(set: &ComParamSet, id: ComParamId) -> &[u8] {
    set.bytes.get(&id).map_or(&[], Vec::as_slice)
}

/// Returns `true` when `working` and `active` differ on any
/// `PDU_PC_BUSTYPE`-class ComParam (ADR-067's `temp_param_update` guard).
/// Unrelated ComParam differences (e.g. addressing staged in Working for one
/// COP) never trigger this -- only the BUSTYPE-class lists above are
/// compared.
///
/// Compares *effective, `GetComParam`-observable* values via
/// `effective_unum32`/`effective_bytes`, not raw map presence: a value the
/// service itself already reports as unchanged (e.g. an unseeded param read
/// as its default and written straight back) is not a "change" the
/// `temp_param_update` prohibition needs to reject (ISO 22900-2 §9.4.16.2.1
/// c) NOTE 2; ADR-133 amendment, Codex review round 2 on PR #147).
pub(super) fn bustype_params_differ(working: &ComParamSet, active: &ComParamSet) -> bool {
    BUSTYPE_UNUM32
        .iter()
        .any(|&id| effective_unum32(working, id) != effective_unum32(active, id))
        || BUSTYPE_BYTES
            .iter()
            .any(|&id| effective_bytes(working, id) != effective_bytes(active, id))
}

/// Returns `true` when `working` and `active` differ on any
/// `PDU_PC_TESTER_PRESENT`-class ComParam (`TESTER_PRESENT_CLASS_PARAMS`) --
/// the TESTER_PRESENT analog of `bustype_params_differ` above, enforcing
/// ISO 22900-2 §9.4.16.2.1 f)'s `temp_param_update` prohibition for this
/// class (ADR-133; Codex review finding on PR #147).
///
/// `TESTER_PRESENT_CLASS_PARAMS` mixes Unum32- and Bytefield-typed members
/// (`PARAM_TESTER_PRESENT_MSG`/`_EXP_POS_RESP`/`_EXP_NEG_RESP` are
/// Bytefield; the rest are Unum32) -- checking both via `effective_unum32`/
/// `effective_bytes` for every id in the list is safe and simpler than
/// splitting it: a given id is only ever populated in the map matching its
/// real type, so the other helper's lookup is always the shared default for
/// it. Like `bustype_params_differ`, this compares *effective,
/// `GetComParam`-observable* values, not raw map presence (ADR-133
/// amendment, Codex review round 2 on PR #147) -- ISO 22900-2 §9.4.16.2.1
/// c) NOTE 2.
pub(super) fn tester_present_params_differ(working: &ComParamSet, active: &ComParamSet) -> bool {
    TESTER_PRESENT_CLASS_PARAMS.iter().any(|&id| {
        effective_unum32(working, id) != effective_unum32(active, id)
            || effective_bytes(working, id) != effective_bytes(active, id)
    })
}

/// Result of [`apply_bustype_lock`]'s three independently-computed roles
/// (ADR-110, correcting the original single-`effective`-set design after a
/// confirmed regression -- see the ADR's Decision section).
pub(super) struct BustypeLockResolution {
    /// The ComParamSet that may safely be pushed to hardware via
    /// `apply_params_to_hardware`. When locked by another CLL, every
    /// `PDU_PC_BUSTYPE`-class key is stripped **unconditionally** --
    /// regardless of whether it happens to differ from this CLL's own
    /// `active` -- because this CLL's own Working-vs-Active agreement can
    /// never prove a write is safe against another CLL's real,
    /// already-pushed hardware state (`LogicalLinkState::active` has no
    /// cross-CLL sync, so it can go stale with no local signal of that).
    pub(super) hw_set: ComParamSet,
    /// The ComParamSet promoted to `LogicalLinkState::active` (and used for
    /// the tester-present re-arm's gating/resolution/`CP_P2Max` reads).
    /// When locked by another CLL, every BUSTYPE-class key is substituted
    /// with this CLL's own pre-call `active` entry (present or absent) --
    /// this CLL's own bookkeeping stays internally consistent even though
    /// the hardware push for that key never happened.
    pub(super) promote_set: ComParamSet,
    /// `true` iff this CLL actually attempted to change at least one
    /// BUSTYPE-class key -- its own `params` (Working) differs from its own
    /// `active` on that key -- while locked by another CLL. ISO 22900-2
    /// §9.4.16 d)'s trigger for emitting exactly one
    /// `PDU_ERR_EVT_RSC_LOCKED` error event: a CLL whose own Working equals its own Active on every
    /// BUSTYPE key attempted nothing, so no event fires even though those
    /// (self-consistent, but possibly hardware-stale) values are still
    /// excluded from `hw_set`.
    pub(super) rsc_locked: bool,
}

/// Resolves a `CoptUpdateparam` promotion against a live physical-ComParam
/// lock conflict (ADR-110, ISO 22900-2 §9.4.16 d)). See
/// [`BustypeLockResolution`] for what each of the three returned pieces
/// means and why they must be computed independently -- in particular,
/// `hw_set`'s BUSTYPE exclusion is unconditional whenever `locked_by_other`,
/// NOT gated on whether `params` differs from `active`: own-Working-vs-Active
/// agreement is an "did I intend to change this" signal, never a "is this
/// value safe to push to hardware" signal, since `active` is this CLL's own,
/// not-cross-CLL-synced bookkeeping and can already be stale relative to
/// whatever the lock holder has pushed to the real shared hardware.
///
/// When `locked_by_other` is false, both `hw_set` and `promote_set` equal
/// `params` (the Working snapshot bound at `StartComPrimitive` time)
/// unchanged, and `rsc_locked` is `false`.
pub(super) fn apply_bustype_lock(
    params: &ComParamSet,
    active: &ComParamSet,
    locked_by_other: bool,
) -> BustypeLockResolution {
    if !locked_by_other {
        return BustypeLockResolution {
            hw_set: params.clone(),
            promote_set: params.clone(),
            rsc_locked: false,
        };
    }
    let mut hw_set = params.clone();
    let mut promote_set = params.clone();
    for &id in BUSTYPE_UNUM32 {
        hw_set.unum32.remove(&id);
        match active.unum32.get(&id) {
            Some(&v) => {
                promote_set.unum32.insert(id, v);
            }
            None => {
                promote_set.unum32.remove(&id);
            }
        }
    }
    for &id in BUSTYPE_BYTES {
        hw_set.bytes.remove(&id);
        match active.bytes.get(&id) {
            Some(v) => {
                promote_set.bytes.insert(id, v.clone());
            }
            None => {
                promote_set.bytes.remove(&id);
            }
        }
    }
    BustypeLockResolution {
        hw_set,
        promote_set,
        rsc_locked: bustype_params_differ(params, active),
    }
}

/// Strips every `PDU_PC_BUSTYPE`-class key (`BUSTYPE_UNUM32`/`BUSTYPE_BYTES`)
/// from a clone of `params`, leaving every other key (including
/// `structfield`, never BUSTYPE-class) unchanged (ADR-110).
///
/// Used to keep a `temp_param_update` bracket (`ParamBinding::Temp`) from
/// ever pushing a BUSTYPE value to hardware -- ISO 22900-2 §9.4.16.2.1 c)
/// NOTE 2 / d): physical ComParams can never be changed via
/// `TempParamUpdate`, lock or no lock, since this CLL's own `active` (the
/// basis for both the temp apply and the eventual revert) can be stale
/// relative to another CLL's real, already-pushed hardware state regardless
/// of whether any lock is currently held. ADR-067 §E's unchanged
/// `PDU_ERR_TEMPPARAM_NOT_ALLOWED` guard already guarantees the bound
/// `effective` snapshot's BUSTYPE portion equals this CLL's own Active, so
/// this strip is a pure no-op from this CLL's own point of view -- but a
/// real fix against the stale-Active clobber, so no lock check and no error
/// event apply to this path at all.
///
/// This strip -- not `bustype_params_differ` -- is the actual boundary that
/// keeps a BUSTYPE key off the wire during a `temp_param_update` bracket.
/// `bustype_params_differ` (ADR-133 amendment) compares *effective,
/// `GetComParam`-observable* values and can therefore correctly report "no
/// change" for an explicit Working entry that happens to equal Active's
/// effective default (e.g. a round-tripped unseeded param) -- but this strip
/// still runs unconditionally regardless of what the differ found, so that
/// under-triggering can never let a BUSTYPE key reach hardware (verified:
/// PR #147 round-3 Codex review, declined as a false positive on exactly
/// this reasoning; see ADR-133's round-3 amendment for the full trace).
pub(super) fn strip_bustype_keys(params: &ComParamSet) -> ComParamSet {
    let mut stripped = params.clone();
    for &id in BUSTYPE_UNUM32 {
        stripped.unum32.remove(&id);
    }
    for &id in BUSTYPE_BYTES {
        stripped.bytes.remove(&id);
    }
    stripped
}

/// Strips a `CHANNEL_WIDE_UNUM32` key from a clone of `params`, but only
/// when `captured_cfgs` shows a successfully-captured restore pair actually
/// exists for it -- the same shape as [`strip_bustype_keys`] above, for a
/// distinct, deliberately disjoint classification (round 18, Codex review,
/// P2, PR #101, ADR-192 Decision item 3 amendment), corrected (edge-case-
/// hunter adversarial review, PR #101, BLOCKING Finding 1) from an earlier,
/// unconditional version that stripped every `CHANNEL_WIDE_UNUM32` key
/// regardless of whether `capture_channel_wide_hardware_locked` actually
/// captured a restore pair for it: on a `GET_CONFIG` capture failure (a real
/// possibility -- J2534-2 Table 77 params are optional, and some adapters
/// accept `SET_CONFIG` but reject the matching `GET_CONFIG`), that left the
/// key with NEITHER an Active-derived restore (stripped from the push)
/// NOR a captured one (capture failed), so the bracket's own temp-bound
/// value stayed live on hardware forever. A key with no captured pair now
/// falls back to its pre-round-18 behavior instead (left in `params` here,
/// so it is restored from `active` by the caller's push, same as every
/// other non-channel-wide key) -- strictly safer than leaking the temp
/// value, even though it revives the "per-CLL Active can be stale" risk
/// this fix otherwise closes, for that one key, on that one failure path
/// only.
///
/// `captured_cfgs` is the set of native `config_id`s actually present in a
/// `channel_wide_restore` slice (see call site), resolved per key via
/// `ComParamId::to_j2534_config_id(hw_protocol_id)` -- not simply "is this
/// id a member of `CHANNEL_WIDE_UNUM32`" as the prior, unconditional
/// version checked.
///
/// Used by `events.rs`'s `revert_hardware_to_live_active_locked` to keep a
/// `temp_param_update` bracket's revert from pushing THIS CLL's own per-CLL
/// Active value for a channel-wide, hardware-resident key back to hardware
/// when a trustworthy captured replacement exists -- Active for one of
/// these keys is exactly the untrustworthy, not-cross-CLL-synced source the
/// capture-and-restore fix (`events.rs`'s
/// `capture_channel_wide_hardware_locked`) exists to bypass when it can;
/// see `CHANNEL_WIDE_UNUM32`'s own doc comment for the full design.
pub(super) fn strip_captured_channel_wide_keys(
    params: &ComParamSet,
    hw_protocol_id: u32,
    captured_cfgs: &HashSet<u32>,
) -> ComParamSet {
    let mut stripped = params.clone();
    for &id in CHANNEL_WIDE_UNUM32 {
        if id
            .to_j2534_config_id(hw_protocol_id)
            .is_some_and(|cfg| captured_cfgs.contains(&cfg))
        {
            stripped.unum32.remove(&id);
        }
    }
    stripped
}

/// Change-only forward for `CP_AnalogActiveChannels`/`CP_AnalogAveragingMethod`
/// (ADR-216 Decision item 10 amendment; Codex review, PR #130, Finding 2):
/// mirrors [`strip_captured_channel_wide_keys`]'s shape exactly -- a targeted
/// strip of specific keys out of `hw_set` before it reaches
/// `apply_params_to_hardware_locked`, rather than a value-comparison guard
/// living in `rpc_primitive.rs` (that shape is already taken, by the
/// permanent `CP_AnalogSamplesPerReading`/`CP_AnalogReadingsPerMsg`
/// exclusion this same Decision item's original fix established) -- but for
/// a distinct, deliberately narrower purpose: these two params stay
/// genuinely live-changeable via `CoptUpdateparam` (ADR-216 Decision item
/// 10's own "no re-stage restriction" framing for `CP_AnalogActiveChannels`,
/// extended to `CP_AnalogAveragingMethod`), so they must never be
/// permanently excluded the way the two batching params are. Instead, only
/// a value THIS CLL is not actually changing (its own currently-staged
/// `hw_set` entry has the same *effective* value, per [`effective_unum32`],
/// as its own current `active`) is stripped -- a genuine change always
/// forwards.
///
/// Closes a cross-sibling stale-value clobber `apply_params_to_hardware_
/// locked`'s blanket-forward would otherwise cause: `handle_update_param`
/// forwards every `unum32` key present in `hw_set`, not a delta of what
/// this COP actually changed, so a sibling CLL sharing the channel that
/// never touched either param would otherwise re-push its own stale copy on
/// every unrelated `CoptUpdateparam` it issues, silently reverting a live
/// change another CLL just made (or, if the connect-time readback ever
/// failed, re-forwarding an absent/`0` `CP_AnalogActiveChannels` -- per
/// clause 10.3.3.2.1's bitmask semantics, deactivating every analog channel
/// on the device). Only `active` is compared against (never `promote_set`,
/// which becomes the new Active regardless of this strip) -- this function
/// changes only what reaches hardware, never this CLL's own bookkeeping.
pub(super) fn strip_unchanged_analog_channel_wide_keys(
    hw_set: &mut ComParamSet,
    active: &ComParamSet,
) {
    for &id in [PARAM_ANALOG_ACTIVE_CHANNELS, PARAM_ANALOG_AVERAGING_METHOD].iter() {
        if effective_unum32(hw_set, id) == effective_unum32(active, id) {
            hw_set.unum32.remove(&id);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_baudrate_allowed_for_all_protocols() {
        for proto in [
            ChannelProtocol::CAN,
            ChannelProtocol::ISO15765,
            ChannelProtocol::ISO9141,
            ChannelProtocol::ISO14230,
            ChannelProtocol::J1850VPW,
            ChannelProtocol::J1850PWM,
            ChannelProtocol::SCI_A_ENGINE,
            ChannelProtocol::SCI_A_TRANS,
        ] {
            assert!(
                check_param_allowed(proto, ComParamId(j2534_0404::DATA_RATE), None,).is_ok(),
                "DATA_RATE should be allowed for protocol {:#x}",
                proto.value()
            );
        }
    }

    #[test]
    fn can_bit_sample_point_only_for_can_family() {
        assert!(
            check_param_allowed(
                ChannelProtocol::CAN,
                ComParamId(j2534_0404::BIT_SAMPLE_POINT),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO15765,
                ComParamId(j2534_0404::BIT_SAMPLE_POINT),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO9141,
                ComParamId(j2534_0404::BIT_SAMPLE_POINT),
                None,
            )
            .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO14230,
                ComParamId(j2534_0404::BIT_SAMPLE_POINT),
                None,
            )
            .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::J1850VPW,
                ComParamId(j2534_0404::BIT_SAMPLE_POINT),
                None,
            )
            .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::J1850PWM,
                ComParamId(j2534_0404::BIT_SAMPLE_POINT),
                None,
            )
            .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::SCI_A_ENGINE,
                ComParamId(j2534_0404::BIT_SAMPLE_POINT),
                None,
            )
            .is_err()
        );
    }

    /// ADR-158/Phase 3a regression test: `CP_CANFDTxMaxDataLength` was
    /// seeded in `comparam_defaults.rs` but absent from `is_can_param`'s
    /// allow-list -- `SetComParam` rejected it outright before this fix.
    /// `PARAM_CANFD_TX_MAX_DATA_LENGTH == ComParamId(0x80BA)`, distinct from
    /// this list's other CAN FD params (`PARAM_CANFD_BAUDRATE` etc.), so
    /// this asserts the specific value that was missing, not just "some CAN
    /// FD param is allowed" (which `PARAM_CANFD_BAUDRATE` alone would have
    /// already made pass even before this fix).
    #[test]
    fn canfd_tx_max_data_length_is_allowed_for_can_family() {
        assert!(
            check_param_allowed(ChannelProtocol::CAN, PARAM_CANFD_TX_MAX_DATA_LENGTH, None,)
                .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO15765,
                PARAM_CANFD_TX_MAX_DATA_LENGTH,
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO9141,
                PARAM_CANFD_TX_MAX_DATA_LENGTH,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn kwp_p_timers_only_for_kwp_family() {
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO9141,
                ComParamId(j2534_0404::P1_MIN),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO14230,
                ComParamId(j2534_0404::P1_MIN),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(ChannelProtocol::CAN, ComParamId(j2534_0404::P1_MIN), None,)
                .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::J1850VPW,
                ComParamId(j2534_0404::P1_MIN),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn access_timing_params_only_for_iso14230_not_can_or_iso9141() {
        // ADR-146: CP_AccessTiming_Ecu / CP_AccessTimingOverride /
        // CP_ExtendedTiming are scoped to ISO14230 specifically (ISO
        // 22900-2's own default-by-protocol tables list ISO_14230_2/
        // ISO_14230_4 only) -- narrower than the general KWP-family grouping
        // (which also includes ISO9141), and not the CAN family at all
        // (the old, backwards allow-listing this ADR corrects).
        for param in [
            PARAM_ACCESS_TIMING_ECU,
            PARAM_ACCESS_TIMING_OVERRIDE,
            PARAM_EXTENDED_TIMING,
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::ISO14230, param, None).is_ok(),
                "param {:#06x} should be allowed for ISO14230",
                param.0
            );
            assert!(
                check_param_allowed(ChannelProtocol::ISO9141, param, None).is_err(),
                "param {:#06x} should NOT be allowed for ISO9141",
                param.0
            );
            assert!(
                check_param_allowed(ChannelProtocol::CAN, param, None).is_err(),
                "param {:#06x} should NOT be allowed for CAN",
                param.0
            );
            assert!(
                check_param_allowed(ChannelProtocol::ISO15765, param, None).is_err(),
                "param {:#06x} should NOT be allowed for ISO15765",
                param.0
            );
        }
    }

    #[test]
    fn network_line_only_for_j1850pwm() {
        assert!(
            check_param_allowed(
                ChannelProtocol::J1850PWM,
                ComParamId(j2534_0404::NETWORK_LINE),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::J1850VPW,
                ComParamId(j2534_0404::NETWORK_LINE),
                None,
            )
            .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::CAN,
                ComParamId(j2534_0404::NETWORK_LINE),
                None,
            )
            .is_err()
        );
    }

    /// ADR-170/Phase 9: SAE J2534-2 clause 12.3.4.1's closed parameter list
    /// -- only DATA_RATE/LOOPBACK (`is_universal_param`) are allowed on a
    /// `UART_ECHO_BYTE_PS` channel; every CAN/KWP/J1850/SCI-specific param
    /// (represented here by `CP_TesterPresentSendType`, the ComParam
    /// ADR-170's rejected-alternatives section discusses) is rejected.
    #[test]
    fn uart_echo_byte_ps_allows_only_universal_params() {
        assert!(
            check_param_allowed(
                ChannelProtocol::UART_ECHO_BYTE_PS,
                ComParamId(j2534_0404::DATA_RATE),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::UART_ECHO_BYTE_PS,
                ComParamId(j2534_0404::LOOPBACK),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::UART_ECHO_BYTE_PS,
                PARAM_TESTER_PRESENT_SEND_TYPE,
                None,
            )
            .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::UART_ECHO_BYTE_PS,
                ComParamId(j2534_0404::P1_MIN),
                None,
            )
            .is_err()
        );
    }

    /// ADR-174/Phase 10: SAE J2534-2 clause 13.3.3.1's closed parameter list
    /// -- only LOOPBACK/P1_MAX/P3_MIN/P4_MIN are allowed on a
    /// `HONDA_DIAGH_PS` channel. `DATA_RATE` is the key contrast with
    /// `uart_echo_byte_ps_allows_only_universal_params` above: it is
    /// rejected here despite being a universal param for every other
    /// protocol, because clause 13's baud rate is fixed (not
    /// `SetComParam`-configurable). `CP_InitializationSettings` is rejected
    /// too (ADR-174 Decision 4).
    #[test]
    fn honda_diagh_ps_allows_only_its_own_closed_param_list() {
        for allowed in [
            ComParamId(j2534_0404::LOOPBACK),
            ComParamId(j2534_0404::P1_MAX),
            ComParamId(j2534_0404::P3_MIN),
            ComParamId(j2534_0404::P4_MIN),
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::HONDA_DIAGH_PS, allowed, None).is_ok(),
                "{allowed:?} should be allowed"
            );
        }
        for rejected in [
            ComParamId(j2534_0404::DATA_RATE),
            PARAM_INIT_SETTINGS,
            ComParamId(j2534_0404::P1_MIN),
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::HONDA_DIAGH_PS, rejected, None).is_err(),
                "{rejected:?} should be rejected"
            );
        }
    }

    /// ADR-175/Phase 11: SAE J2534-2 clause 17.3.2.2.1's closed parameter
    /// list -- DATA_RATE/LOOPBACK/PARAM_MESSAGE_PRIORITY are allowed on a
    /// `J1708_PS` channel; every CAN/KWP/J1850/SCI-specific param (again
    /// represented by `CP_TesterPresentSendType`) is rejected.
    #[test]
    fn j1708_ps_allows_only_its_own_closed_param_list() {
        for allowed in [
            ComParamId(j2534_0404::DATA_RATE),
            ComParamId(j2534_0404::LOOPBACK),
            PARAM_MESSAGE_PRIORITY,
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::J1708_PS, allowed, None).is_ok(),
                "{allowed:?} should be allowed"
            );
        }
        for rejected in [
            PARAM_TESTER_PRESENT_SEND_TYPE,
            ComParamId(j2534_0404::P1_MIN),
            ComParamId(j2534_0404::P1_MAX),
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::J1708_PS, rejected, None).is_err(),
                "{rejected:?} should be rejected"
            );
        }
    }

    /// ADR-179/Phase 5 (amended by ADR-184): SAE J2534-2 clause 16.3.3.1.1's
    /// general CAN config surface plus J1939-specific/reused-transport-timer
    /// ComParams are allowed on a `J1939_PS` channel; every CAN-addressing/
    /// KWP/J1850/SCI-specific param not in that list (again represented by
    /// `CP_TesterPresentSendType`) is rejected -- as is `CP_J1939SourceAddress`
    /// itself now (ADR-184: `PDU_PC_UNIQUE_ID` class, UniqueRespIdTable-only
    /// per ISO 22900-2 §9.3.3.6, moved out of the "allowed" list below into
    /// "rejected"). `CP_J1939SourceName` stays allowed -- deliberately left
    /// unclassified as `PDU_PC_UNIQUE_ID` this pass (P3 residual).
    #[test]
    fn j1939_ps_allows_only_its_own_closed_param_list() {
        for allowed in [
            ComParamId(j2534_0404::DATA_RATE),
            ComParamId(j2534_0404::LOOPBACK),
            ComParamId(j2534_0404::BIT_SAMPLE_POINT),
            ComParamId(j2534_0404::SYNC_JUMP_WIDTH),
            PARAM_MESSAGE_PRIORITY,
            PARAM_N_CR,
            PARAM_N_CS,
            PARAM_N_BS,
            PARAM_N_BR,
            ComParamId(j2534_0404::T3_MAX),
            ComParamId(j2534_0404::T4_MAX),
            ComParamId(j2534_0404::T5_MAX),
            PARAM_J1939_ADDR_CLAIM_TIMEOUT,
            PARAM_J1939_TARGET_ADDRESS,
            PARAM_J1939_PREFERRED_ADDRESS,
            PARAM_J1939_NAME,
            PARAM_J1939_SOURCE_NAME,
            PARAM_J1939_TARGET_NAME,
            // CP_TesterSourceAddress (alias for NODE_ADDRESS) -- ADR-179
            // Decision 3's claimed-address client-readback contract.
            ComParamId(j2534_0404::NODE_ADDRESS),
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::J1939_PS, allowed, None).is_ok(),
                "{allowed:?} should be allowed"
            );
        }
        for rejected in [
            PARAM_TESTER_PRESENT_SEND_TYPE,
            ComParamId(j2534_0404::P1_MIN),
            ComParamId(j2534_0404::P1_MAX),
            // ADR-184: now PDU_PC_UNIQUE_ID class, UniqueRespIdTable-only.
            PARAM_J1939_SOURCE_ADDRESS,
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::J1939_PS, rejected, None).is_err(),
                "{rejected:?} should be rejected"
            );
        }
    }

    /// ADR-188/Phase 7 Stage 7a, extended by ADR-190/Phase 7 Stage 7b, then
    /// ADR-192/Phase 7 Stage 7c: `TP2_0_PS` allows its own closed param list
    /// -- the general CAN config surface, the five Stage 7a
    /// active-connection `PARAM_TP20_*` ComParams, Stage 7b's two
    /// passive-connection ones, and Stage 7c's two broadcast ones -- and
    /// rejects everything else, including `CP_TesterPresentSendType` (ADR-188
    /// Decision item 4).
    #[test]
    fn tp2_0_ps_allows_only_its_own_closed_param_list() {
        for allowed in [
            ComParamId(j2534_0404::DATA_RATE),
            ComParamId(j2534_0404::LOOPBACK),
            ComParamId(j2534_0404::BIT_SAMPLE_POINT),
            ComParamId(j2534_0404::SYNC_JUMP_WIDTH),
            PARAM_TP20_CHANNEL_SETUP_CAN_ID,
            PARAM_TP20_DESTINATION_ADDRESS,
            PARAM_TP20_TX_ID_PROPOSAL,
            PARAM_TP20_RX_ID_PROPOSAL,
            PARAM_TP20_APPLICATION_TYPE,
            PARAM_TP20_PASSIVE_IDENTIFIER,
            PARAM_TP20_PASSIVE_RX_ID,
            PARAM_TP20_BROADCAST_ADDRESS,
            PARAM_TP20_BROADCAST_INTERVAL,
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::TP2_0_PS, allowed, None).is_ok(),
                "{allowed:?} should be allowed"
            );
        }
        for rejected in [
            PARAM_TESTER_PRESENT_SEND_TYPE,
            ComParamId(j2534_0404::P1_MIN),
            ComParamId(j2534_0404::P1_MAX),
            PARAM_J1939_SOURCE_ADDRESS,
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::TP2_0_PS, rejected, None).is_err(),
                "{rejected:?} should be rejected"
            );
        }
    }

    /// ADR-194/Phase 16: `ETHERNET_NDIS` allows exactly one param,
    /// `PARAM_NDIS_PIN_OPTION` (`CP_NdisPinOption`) -- not even
    /// `DATA_RATE`/`LOOPBACK`, since clause 24 defines no native
    /// ComParam-shaped concept at all.
    #[test]
    fn ethernet_ndis_allows_only_ndis_pin_option() {
        assert!(
            check_param_allowed(ChannelProtocol::ETHERNET_NDIS, PARAM_NDIS_PIN_OPTION, None)
                .is_ok(),
            "PARAM_NDIS_PIN_OPTION should be allowed"
        );
        for rejected in [
            ComParamId(j2534_0404::DATA_RATE),
            ComParamId(j2534_0404::LOOPBACK),
            PARAM_TESTER_PRESENT_SEND_TYPE,
            PARAM_ANALOG_SAMPLE_RATE,
        ] {
            assert!(
                check_param_allowed(ChannelProtocol::ETHERNET_NDIS, rejected, None).is_err(),
                "{rejected:?} should be rejected"
            );
        }
        // The inverse: `PARAM_NDIS_PIN_OPTION` must not leak through on an
        // unrelated protocol via the "Unknown protocol — allow" fallback.
        assert!(
            check_param_allowed(ChannelProtocol::CAN, PARAM_NDIS_PIN_OPTION, None).is_err(),
            "PARAM_NDIS_PIN_OPTION should be rejected for a non-Ethernet_NDIS protocol"
        );
    }

    /// Backlog fix (closes j2534-0404-service/docs/implementation-notes.md's
    /// "CP_PhysReqFormatPriorityType" prioritized-backlog bullet):
    /// `tx_header::j1850_header_bytes` reads `CP_PhysReqFormatPriorityType`/
    /// `CP_PhysReqTargetAddr`/`CP_FuncReqFormatPriorityType`/
    /// `CP_FuncReqTargetAddr`/`NODE_ADDRESS` to build the outgoing J1850
    /// addressing header (ADR-050/ADR-054), but `is_j1850pwm_param`/
    /// `is_j1850vpw_param` omitted all five from their allow-lists --
    /// `SetComParam` rejected every one of them on a J1850 CLL, unlike the
    /// identical entries `is_can_param`/`is_kwp_param` already allow for
    /// CAN/KWP.
    #[test]
    fn j1850_addressing_params_allowed_for_both_j1850_variants() {
        for proto in [ChannelProtocol::J1850PWM, ChannelProtocol::J1850VPW] {
            for param in [
                PARAM_PHYS_REQ_FORMAT_PRIORITY,
                PARAM_PHYS_REQ_TARGET_ADDR,
                PARAM_FUNC_REQ_FORMAT_PRIORITY,
                PARAM_FUNC_REQ_TARGET_ADDR,
                ComParamId(j2534_0404::NODE_ADDRESS),
            ] {
                assert!(
                    check_param_allowed(proto, param, None).is_ok(),
                    "param {:#06x} should be allowed for J1850 protocol {:#x}",
                    param.0,
                    proto.value()
                );
            }
        }
    }

    #[test]
    fn enable_concatenation_only_for_kwp_and_j1850_family() {
        // ISO 22900-2:2022 Table B.11: CP_EnableConcatenation applies to the
        // KWP family (ISO 9141-2, ISO 14230-2/-4) and SAE J1850 VPW/PWM
        // only, not CAN/ISO15765 or SCI (the 2009(E) edition disagreed --
        // see ADR-148).
        assert!(
            check_param_allowed(ChannelProtocol::ISO9141, PARAM_ENABLE_CONCATENATION, None,)
                .is_ok()
        );
        assert!(
            check_param_allowed(ChannelProtocol::ISO14230, PARAM_ENABLE_CONCATENATION, None,)
                .is_ok()
        );
        assert!(
            check_param_allowed(ChannelProtocol::J1850VPW, PARAM_ENABLE_CONCATENATION, None,)
                .is_ok()
        );
        assert!(
            check_param_allowed(ChannelProtocol::J1850PWM, PARAM_ENABLE_CONCATENATION, None,)
                .is_ok()
        );
        assert!(
            check_param_allowed(ChannelProtocol::CAN, PARAM_ENABLE_CONCATENATION, None,).is_err()
        );
        assert!(
            check_param_allowed(ChannelProtocol::ISO15765, PARAM_ENABLE_CONCATENATION, None,)
                .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::SCI_A_ENGINE,
                PARAM_ENABLE_CONCATENATION,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn sci_timing_only_for_sci_family() {
        assert!(
            check_param_allowed(
                ChannelProtocol::SCI_A_ENGINE,
                ComParamId(j2534_0404::T1_MAX),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::SCI_B_TRANS,
                ComParamId(j2534_0404::T1_MAX),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(ChannelProtocol::CAN, ComParamId(j2534_0404::T1_MAX), None,)
                .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO9141,
                ComParamId(j2534_0404::T1_MAX),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn iso15765_flow_control_for_can_family_only() {
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO15765,
                ComParamId(j2534_0404::ISO15765_BS),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::CAN,
                ComParamId(j2534_0404::ISO15765_BS),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO9141,
                ComParamId(j2534_0404::ISO15765_BS),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn can_service_params_only_for_can_family() {
        // CP_CanFuncReqId is a shared functional address (not PDU_PC_UNIQUE_ID
        // class), so it goes through Get/SetComParam like any other CAN param.
        assert!(check_param_allowed(ChannelProtocol::CAN, PARAM_CAN_FUNC_REQ_ID, None,).is_ok());
        assert!(
            check_param_allowed(ChannelProtocol::ISO9141, PARAM_CAN_FUNC_REQ_ID, None,).is_err()
        );
        assert!(
            check_param_allowed(ChannelProtocol::J1850VPW, PARAM_CAN_FUNC_REQ_ID, None,).is_err()
        );
    }

    #[test]
    fn unique_id_class_params_rejected_by_get_set_com_param() {
        // ISO 22900-2 §9.3.3.6: PDU_PC_UNIQUE_ID class params are managed
        // exclusively via Get/SetUniqueRespIdTable, never Get/SetComParam —
        // even on the protocol family they otherwise belong to.
        assert!(check_param_allowed(ChannelProtocol::CAN, PARAM_CAN_PHYS_REQ_ID, None,).is_err());
        assert!(
            check_param_allowed(ChannelProtocol::ISO15765, PARAM_CAN_PHYS_REQ_ID, None,).is_err()
        );
        assert!(check_param_allowed(ChannelProtocol::CAN, PARAM_CAN_RESP_USDT_ID, None,).is_err());
        assert!(check_param_allowed(ChannelProtocol::CAN, PARAM_CAN_RESP_UUDT_ID, None,).is_err());
        assert!(
            check_param_allowed(ChannelProtocol::CAN, PARAM_J1939_SOURCE_ADDRESS, None,).is_err()
        );
        assert!(check_param_allowed(ChannelProtocol::CAN, PARAM_J1939_SOURCE_NAME, None,).is_err());
        // ADR-184: on ITS OWN protocol (J1939_PS, unlike the CAN-family
        // misclassification checked above), CP_J1939SourceAddress is now
        // genuinely PDU_PC_UNIQUE_ID class (J1939_UNIQUE_ID_UNUM32) and
        // rejected here for the first time -- previously (pre-ADR-184)
        // `is_j1939_param` accepted it directly.  CP_J1939SourceName
        // remains a plain settable param for J1939_PS (P3 residual, not
        // PDU_PC_UNIQUE_ID class) -- contrast confirms the split is
        // deliberate, not a blanket J1939 rejection.
        assert!(
            check_param_allowed(ChannelProtocol::J1939_PS, PARAM_J1939_SOURCE_ADDRESS, None,)
                .is_err()
        );
        assert!(
            check_param_allowed(ChannelProtocol::J1939_PS, PARAM_J1939_SOURCE_NAME, None,).is_ok()
        );
        assert!(
            check_param_allowed(ChannelProtocol::ISO9141, PARAM_ECU_RESP_SOURCE_ADDR, None,)
                .is_err()
        );
        assert!(check_param_allowed(ChannelProtocol::ISO14230, PARAM_MID_RESP_ID, None,).is_err());
        assert!(check_param_allowed(ChannelProtocol::J1850PWM, PARAM_MID_RESP_ID, None,).is_err());
        assert!(check_param_allowed(ChannelProtocol::J1850VPW, PARAM_MID_RESP_ID, None,).is_err());
        // ADR-202: the 4 params newly reclassified onto J1850's UNIQUE_ID
        // list are, like every other PDU_PC_UNIQUE_ID-class param, still
        // rejected by plain Get/SetComParam for both J1850 protocol ids --
        // only the gate they fall through changed (is_unique_id_param
        // instead of the general J1850 allow-list; neither list ever
        // accepted these 4 params, so there's no observable behavior
        // change on this path).
        for proto in [ChannelProtocol::J1850PWM, ChannelProtocol::J1850VPW] {
            assert!(check_param_allowed(proto, PARAM_ECU_RESP_SOURCE_ADDR, None,).is_err());
            assert!(check_param_allowed(proto, PARAM_FUNC_RESP_FORMAT_PRIORITY, None,).is_err());
            assert!(check_param_allowed(proto, PARAM_FUNC_RESP_TARGET_ADDR, None,).is_err());
            assert!(check_param_allowed(proto, PARAM_PHYS_RESP_FORMAT_PRIORITY, None,).is_err());
        }

        // Request-side / non-unique counterparts are unaffected.
        assert!(check_param_allowed(ChannelProtocol::CAN, PARAM_CAN_FUNC_REQ_ID, None,).is_ok());
        assert!(
            check_param_allowed(ChannelProtocol::CAN, PARAM_J1939_TARGET_ADDRESS, None,).is_ok()
        );
        assert!(check_param_allowed(ChannelProtocol::ISO9141, PARAM_MID_REQ_ID, None,).is_ok());
    }

    #[test]
    fn j1939_params_only_for_can_family() {
        assert!(check_param_allowed(ChannelProtocol::CAN, PARAM_J1939_NAME, None,).is_ok());
        assert!(check_param_allowed(ChannelProtocol::ISO15765, PARAM_J1939_NAME, None,).is_ok());
        assert!(check_param_allowed(ChannelProtocol::ISO9141, PARAM_J1939_NAME, None,).is_err());
    }

    #[test]
    fn tester_present_for_can_and_kwp_not_sci() {
        assert!(check_param_allowed(ChannelProtocol::CAN, PARAM_TESTER_PRESENT_MSG, None,).is_ok());
        assert!(
            check_param_allowed(ChannelProtocol::ISO14230, PARAM_TESTER_PRESENT_MSG, None,).is_ok()
        );
        assert!(
            check_param_allowed(ChannelProtocol::J1850PWM, PARAM_TESTER_PRESENT_MSG, None,).is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::SCI_A_ENGINE,
                PARAM_TESTER_PRESENT_MSG,
                None,
            )
            .is_ok()
        );
    }

    #[test]
    fn uart_data_bits_for_kwp_and_sci_not_can() {
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO9141,
                ComParamId(j2534_0404::DATA_BITS),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::ISO14230,
                ComParamId(j2534_0404::DATA_BITS),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::SCI_A_ENGINE,
                ComParamId(j2534_0404::DATA_BITS),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::CAN,
                ComParamId(j2534_0404::DATA_BITS),
                None,
            )
            .is_err()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::J1850PWM,
                ComParamId(j2534_0404::DATA_BITS),
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn unknown_protocol_allows_all() {
        // Protocol 0xFFFF is unknown — allow everything for forward-compat.
        assert!(
            check_param_allowed(
                ChannelProtocol::from_raw(0xFFFF),
                ComParamId(j2534_0404::BIT_SAMPLE_POINT),
                None,
            )
            .is_ok()
        );
        assert!(
            check_param_allowed(
                ChannelProtocol::from_raw(0xFFFF),
                PARAM_CAN_PHYS_REQ_ID,
                None,
            )
            .is_ok()
        );
    }

    /// Codex review, PR #70 round 3: unlike every other ComParam, an unknown/
    /// custom raw protocol id must NOT allow `CP_AnalogSampleRate` -- the
    /// ANALOG_IN branch's own `param_id == PARAM_ANALOG_SAMPLE_RATE` check
    /// only ever runs for a resolved native `PROTOCOL_ANALOG_IN_x` id, so
    /// without an explicit exclusion this param would otherwise fall through
    /// to the "Unknown protocol — allow" catch-all `unknown_protocol_allows_all`
    /// exercises for every other param. Staging it on a non-analog CLL would
    /// then be silently ignored at `ConnectComLogicalLink` time (only
    /// `is_analog_in_protocol_id` links read/apply it), while `GetComParam`
    /// kept reporting it as set -- a regression from the removed
    /// `CreateComLogicalLinkRequest.analog_sample_rate` field's own explicit
    /// nonzero-on-non-analog rejection (ADR-177). Confirmed to genuinely fail
    /// without the fix (a temporary local patch removing the exclusion made
    /// this test panic on a missing rejection).
    #[test]
    fn unknown_protocol_still_rejects_cp_analog_sample_rate() {
        assert!(
            check_param_allowed(
                ChannelProtocol::from_raw(0xFFFF),
                PARAM_ANALOG_SAMPLE_RATE,
                None,
            )
            .is_err()
        );
    }

    // ── unique_id_params ──────────────────────────────────────────────────────

    #[test]
    fn unique_id_params_can_includes_all_nine_can_addressing_params() {
        let (unum32, _bytes) = unique_id_params(ChannelProtocol::CAN);
        assert!(unum32.contains(&PARAM_CAN_PHYS_REQ_ID));
        assert!(unum32.contains(&PARAM_CAN_RESP_USDT_ID));
        assert!(unum32.contains(&PARAM_CAN_RESP_UUDT_ID));
        assert!(unum32.contains(&PARAM_CAN_PHYS_REQ_EXT_ADDR));
        assert!(unum32.contains(&PARAM_CAN_PHYS_REQ_FORMAT));
        assert!(unum32.contains(&PARAM_CAN_RESP_USDT_EXT_ADDR));
        assert!(unum32.contains(&PARAM_CAN_RESP_USDT_FORMAT));
        assert!(unum32.contains(&PARAM_CAN_RESP_UUDT_EXT_ADDR));
        assert!(unum32.contains(&PARAM_CAN_RESP_UUDT_FORMAT));
    }

    #[test]
    fn unique_id_params_can_includes_ecu_addressing_params() {
        let (unum32, _bytes) = unique_id_params(ChannelProtocol::CAN);
        assert!(unum32.contains(&PARAM_ECU_RESP_SOURCE_ADDR));
        assert!(unum32.contains(&PARAM_FUNC_RESP_FORMAT_PRIORITY));
        assert!(unum32.contains(&PARAM_FUNC_RESP_TARGET_ADDR));
        assert!(unum32.contains(&PARAM_PHYS_RESP_FORMAT_PRIORITY));
        assert!(unum32.contains(&PARAM_MID_RESP_ID));
    }

    /// ADR-184: `PARAM_J1939_SOURCE_ADDRESS`/`PARAM_J1939_SOURCE_NAME` are no
    /// longer (mis)classified as CAN-family `PDU_PC_UNIQUE_ID` params --
    /// `CP_J1939SourceAddress` now belongs to `J1939_PS`'s own list instead
    /// (see `unique_id_params_j1939_includes_source_address_only`), and
    /// `CP_J1939SourceName` isn't `PDU_PC_UNIQUE_ID` class for any protocol
    /// this pass (P3 residual). CAN's own bytefield list is empty as a
    /// result -- it never had any other entry.
    #[test]
    fn unique_id_params_can_excludes_j1939_params_and_bytes_is_empty() {
        let (unum32, bytes) = unique_id_params(ChannelProtocol::CAN);
        assert!(!unum32.contains(&PARAM_J1939_SOURCE_ADDRESS));
        assert!(bytes.is_empty());
    }

    /// ADR-184: `J1939_PS` now has its
    /// own `PDU_PC_UNIQUE_ID` unum32 list (`CP_J1939SourceAddress` only; no
    /// bytefield entry, `CP_J1939SourceName` deliberately excluded, P3
    /// residual). `unique_id_params`'s own `_CH1..128` dispatch
    /// (`resources::is_j1939_protocol_id`) is exercised directly by that
    /// function's own tests (`resources.rs`); `ChannelProtocol`'s private
    /// inner value means a `_CHx` id cannot be constructed from this test
    /// module to exercise it here too.
    #[test]
    fn unique_id_params_j1939_includes_source_address_only() {
        let (unum32, bytes) = unique_id_params(ChannelProtocol::J1939_PS);
        assert_eq!(unum32, &[PARAM_J1939_SOURCE_ADDRESS]);
        assert!(bytes.is_empty());
        assert!(!unum32.contains(&PARAM_J1939_SOURCE_NAME));
        assert!(!bytes.contains(&PARAM_J1939_SOURCE_NAME));
    }

    #[test]
    fn unique_id_params_iso15765_same_as_can() {
        let (can_u, can_b) = unique_id_params(ChannelProtocol::CAN);
        let (iso_u, iso_b) = unique_id_params(ChannelProtocol::ISO15765);
        assert_eq!(can_u, iso_u);
        assert_eq!(can_b, iso_b);
    }

    #[test]
    fn unique_id_params_kwp_includes_ecu_addressing_only() {
        let (unum32, bytes) = unique_id_params(ChannelProtocol::ISO9141);
        assert!(unum32.contains(&PARAM_ECU_RESP_SOURCE_ADDR));
        assert!(unum32.contains(&PARAM_MID_RESP_ID));
        // KWP does not have CAN addressing or J1939 params
        assert!(!unum32.contains(&PARAM_CAN_PHYS_REQ_ID));
        assert!(!unum32.contains(&PARAM_J1939_SOURCE_ADDRESS));
        assert!(bytes.is_empty());
    }

    #[test]
    fn unique_id_params_iso14230_same_as_iso9141() {
        let (kwp_u, kwp_b) = unique_id_params(ChannelProtocol::ISO9141);
        let (iso_u, iso_b) = unique_id_params(ChannelProtocol::ISO14230);
        assert_eq!(kwp_u, iso_u);
        assert_eq!(kwp_b, iso_b);
    }

    #[test]
    fn check_param_allowed_rejection_carries_error_detail() {
        use vci_service_interface::{PduError, PduErrorEvent, error_detail_from_status};

        let status =
            check_param_allowed(ChannelProtocol::CAN, ComParamId(j2534_0404::P1_MIN), None)
                .unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        let detail = error_detail_from_status(&status).expect("ErrorDetail should be attached");
        assert_eq!(
            detail.pdu_error,
            PduError::PduErrComparamNotSupported as i32
        );
        assert!(detail.error_event_data.is_none());

        let status_with_tracked_error = check_param_allowed(
            ChannelProtocol::CAN,
            ComParamId(j2534_0404::P1_MIN),
            Some(TrackedError {
                event: PduErrorEvent::PduErrEvtLostCommToVci,
                timestamp: 7,
                cop: None,
                cop_tag: None,
            }),
        )
        .unwrap_err();
        let detail = error_detail_from_status(&status_with_tracked_error)
            .expect("ErrorDetail should be attached");
        let event_data = detail.error_event_data.expect("event data should be set");
        assert_eq!(
            event_data.error_event,
            PduErrorEvent::PduErrEvtLostCommToVci as i32
        );
        assert_eq!(event_data.timestamp, 7);
    }

    /// edge-case-hunter finding, PR #22 close-out: `is_param_allowed`'s
    /// `CP_AccessTiming_Ecu`/`CP_AccessTimingOverride`/`CP_ExtendedTiming`
    /// gate must recognize the SAME extended KWP protocols ADR-150's
    /// `kwp_access_timing_applies()` does, or the mechanism silently
    /// activates for a protocol whose client can never `SetComParam`/
    /// `GetComParam` the very ComParams it reads/writes.
    ///
    /// Fail-without-the-fix control: reverting this arm to
    /// `protocol == ChannelProtocol::ISO14230` (this test's pre-fix shape)
    /// was confirmed to make the `ISO_14230_3_ON_ISO_14230_2`/
    /// `ISO_15031_5_ON_ISO_14230_4` assertions below fail.
    #[test]
    fn access_timing_structfield_params_allowed_on_every_kwp_access_timing_protocol() {
        for protocol in [
            ChannelProtocol::ISO14230,
            ChannelProtocol::ISO_14230_3_ON_ISO_14230_2,
            ChannelProtocol::ISO_15031_5_ON_ISO_14230_4,
        ] {
            assert!(
                protocol.kwp_access_timing_applies(),
                "test precondition: {protocol:?} must be in kwp_access_timing_applies()'s set"
            );
            for param in [
                PARAM_ACCESS_TIMING_ECU,
                PARAM_ACCESS_TIMING_OVERRIDE,
                PARAM_EXTENDED_TIMING,
            ] {
                assert!(
                    check_param_allowed(protocol, param, None).is_ok(),
                    "{param:?} must be allowed on {protocol:?} -- ADR-146's live-exchange \
                     mechanism (ADR-150's kwp_access_timing_applies()) is active there"
                );
            }
        }

        // ISO9141 is in the wider KWP family but has no Access Timing
        // Parameter service equivalent (ADR-146's own original scoping) --
        // must still reject, confirming this fix didn't over-widen the gate.
        assert!(
            check_param_allowed(ChannelProtocol::ISO9141, PARAM_ACCESS_TIMING_ECU, None).is_err()
        );
    }

    /// ADR-202: ISO 22900-2:2022 Table B.11 classifies these 5 unum32 params
    /// (the same 4 response-addressing params as `KWP_UNIQUE_ID_UNUM32`, plus
    /// the retained `CP_MidRespId` surplus entry -- see `J1850_UNIQUE_ID_UNUM32`'s
    /// own doc comment) as `PDU_PC_UNIQUE_ID` class for both J1850 protocol
    /// ids. No bytefield entry: no J1850-scoped bytefield UNIQUE_ID param
    /// exists in Table B.11.
    #[test]
    fn unique_id_params_j1850_includes_full_response_addressing_set() {
        for proto in [ChannelProtocol::J1850PWM, ChannelProtocol::J1850VPW] {
            let (unum32, bytes) = unique_id_params(proto);
            assert_eq!(
                unum32,
                &[
                    PARAM_ECU_RESP_SOURCE_ADDR,
                    PARAM_FUNC_RESP_FORMAT_PRIORITY,
                    PARAM_FUNC_RESP_TARGET_ADDR,
                    PARAM_PHYS_RESP_FORMAT_PRIORITY,
                    PARAM_MID_RESP_ID,
                ],
                "proto {:?}",
                proto
            );
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn unique_id_params_sci_is_empty() {
        for proto in [
            ChannelProtocol::SCI_A_ENGINE,
            ChannelProtocol::SCI_A_TRANS,
            ChannelProtocol::SCI_B_ENGINE,
            ChannelProtocol::SCI_B_TRANS,
        ] {
            let (unum32, bytes) = unique_id_params(proto);
            assert!(
                unum32.is_empty(),
                "SCI proto {:?} should have no UNIQUE_ID params",
                proto
            );
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn unique_id_params_extended_can_protocol_resolves_to_can_family() {
        // ISO_14229_3_ON_ISO_15765_2 uses ISO15765 (CAN family) at the hardware level.
        let (unum32, _bytes) = unique_id_params(ChannelProtocol::ISO_14229_3_ON_ISO_15765_2);
        assert!(unum32.contains(&PARAM_CAN_RESP_USDT_ID));
    }

    // ── bustype_params_differ (ADR-067) ──────────────────────────────────────

    #[test]
    fn bustype_params_differ_false_when_identical() {
        let mut a = ComParamSet::default();
        a.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 500_000);
        a.unum32
            .insert(ComParamId(j2534_0404::BIT_SAMPLE_POINT), 80);
        let b = a.clone();
        assert!(!bustype_params_differ(&a, &b));
    }

    #[test]
    fn bustype_params_differ_true_on_baud_rate_change() {
        let mut working = ComParamSet::default();
        working
            .unum32
            .insert(ComParamId(j2534_0404::DATA_RATE), 500_000);
        let mut active = ComParamSet::default();
        active
            .unum32
            .insert(ComParamId(j2534_0404::DATA_RATE), 250_000);
        assert!(bustype_params_differ(&working, &active));
    }

    #[test]
    fn bustype_params_differ_true_on_bit_sample_point_change() {
        let mut working = ComParamSet::default();
        working
            .unum32
            .insert(ComParamId(j2534_0404::BIT_SAMPLE_POINT), 80);
        let active = ComParamSet::default();
        assert!(bustype_params_differ(&working, &active));
    }

    #[test]
    fn bustype_params_differ_true_on_can_baudrate_record_bytefield_change() {
        let mut working = ComParamSet::default();
        working
            .bytes
            .insert(PARAM_CAN_BAUDRATE_RECORD, vec![1, 2, 3]);
        let active = ComParamSet::default();
        assert!(bustype_params_differ(&working, &active));
    }

    #[test]
    fn bustype_params_differ_false_for_non_bustype_param_change() {
        // CP_RequestAddrMode is not BUSTYPE class -- an addressing-mode
        // difference alone must not trip the guard.
        let mut working = ComParamSet::default();
        working.unum32.insert(PARAM_REQUEST_ADDR_MODE, 2);
        let active = ComParamSet::default();
        assert!(!bustype_params_differ(&working, &active));
    }

    // ── PARAM_TP20_BROADCAST_INTERVAL classification regression fences
    // (Codex review round 8 Finding 2, PR #101, ADR-192/Phase 7 Stage 7c):
    // `CP_TP20BroadcastInterval` is deliberately classified like
    // `CP_Cs`/`CONFIG_J1939_BRDCST_MIN_DELAY` -- a per-channel,
    // hardware-resident, but PACING-class ComParam, never added to
    // `BUSTYPE_UNUM32`. ISO 22900-2 §9.4.16.2.1's NOTE on the BUSTYPE class's
    // `temp_param_update` prohibition would otherwise conflict with this
    // service's own shipped, already-tested `temp_param_update=1` per-burst
    // override of this param (`tests/grpc_mock/tp20.rs`'s
    // `broadcast_periodic_temp_param_update_applies_and_reverts_broadcast_interval`).
    // These three tests pin that decision so a future well-intentioned "fix"
    // (adding it to `BUSTYPE_UNUM32` because it is per-channel and
    // hardware-resident) does not silently regress the shipped feature. ────

    #[test]
    fn tp20_broadcast_interval_is_not_a_bustype_param() {
        assert!(
            !is_bustype_param(PARAM_TP20_BROADCAST_INTERVAL),
            "CP_TP20BroadcastInterval must stay out of BUSTYPE_UNUM32/BUSTYPE_BYTES -- it is \
             pacing-class, like CP_Cs/CONFIG_J1939_BRDCST_MIN_DELAY, not bus-configuration-class \
             like DATA_RATE/BIT_SAMPLE_POINT"
        );
    }

    #[test]
    fn bustype_params_differ_false_when_only_tp20_broadcast_interval_differs() {
        let mut working = ComParamSet::default();
        working.unum32.insert(PARAM_TP20_BROADCAST_INTERVAL, 250);
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_TP20_BROADCAST_INTERVAL, 20);
        assert!(
            !bustype_params_differ(&working, &active),
            "a Working/Active difference confined to CP_TP20BroadcastInterval must not trip the \
             BUSTYPE temp_param_update guard -- this is exactly the shipped per-burst \
             temp-override feature (rpc_start_com_primitive's TP2.0 broadcast-periodic branch), \
             which would become illegal if this param were ever added to BUSTYPE_UNUM32"
        );
    }

    #[test]
    fn com_param_class_tp20_broadcast_interval_is_pdu_pc_specified() {
        assert_eq!(
            com_param_class(PARAM_TP20_BROADCAST_INTERVAL),
            vci_service_interface::PduParamClass::PduPcSpecified,
            "CP_TP20BroadcastInterval must report PduPcSpecified (unclassified), not \
             PduPcBustype -- GetComParam's classification must follow is_bustype_param, and this \
             param is deliberately not BUSTYPE-class"
        );
    }

    // ── CHANNEL_WIDE_UNUM32 classification regression fences (round 18,
    // Codex review, P2, PR #101, ADR-192 Decision item 3 amendment): pins
    // CHANNEL_WIDE_UNUM32 and BUSTYPE_UNUM32 as deliberately disjoint
    // classifications, and pins the one key currently in CHANNEL_WIDE_UNUM32,
    // so a future addition to either list cannot silently collide with the
    // other. ────────────────────────────────────────────────────────────────

    #[test]
    fn channel_wide_unum32_disjoint_from_bustype_unum32() {
        assert!(
            !BUSTYPE_UNUM32
                .iter()
                .any(|id| CHANNEL_WIDE_UNUM32.contains(id)),
            "CHANNEL_WIDE_UNUM32 and BUSTYPE_UNUM32 must stay disjoint -- a channel-wide, \
             hardware-resident key that is deliberately NOT BUSTYPE-class (like \
             CP_TP20BroadcastInterval) must never also appear in BUSTYPE_UNUM32"
        );
    }

    #[test]
    fn channel_wide_unum32_contains_only_tp20_broadcast_interval() {
        assert_eq!(
            CHANNEL_WIDE_UNUM32,
            &[PARAM_TP20_BROADCAST_INTERVAL],
            "CHANNEL_WIDE_UNUM32 must currently name exactly CP_TP20BroadcastInterval -- the \
             only channel-wide, hardware-resident, non-BUSTYPE ComParam this service maps today"
        );
    }

    // ── bustype_params_differ / tester_present_params_differ: effective-value
    // fix (ADR-133 amendment, Codex review round 2, PR #147) ─────────────────

    /// `PARAM_CANFD_BAUDRATE` is allowed for the whole CAN family but seeded
    /// only in CAN-FD presets (`comparam_defaults.rs:1456,1476` confirm it is
    /// absent on plain-CAN/29-bit presets). A client that reads the unseeded
    /// default via `GetComParam` (which reports `0`, via `effective_unum32`)
    /// and writes it straight back via `SetComParam` ends up with
    /// `Working = Some(0)`, `Active = None` -- raw map-presence comparison
    /// (the pre-fix behavior) would misreport this as a real BUSTYPE change.
    #[test]
    fn bustype_params_differ_false_on_unum32_zero_roundtrip_for_unseeded_param() {
        let mut working = ComParamSet::default();
        working.unum32.insert(PARAM_CANFD_BAUDRATE, 0);
        let active = ComParamSet::default();
        assert!(
            !bustype_params_differ(&working, &active),
            "an explicit round-tripped 0 for an unseeded BUSTYPE Unum32 param must compare equal \
             to Active's absent entry -- both resolve to the same effective (GetComParam- \
             observable) value"
        );
    }

    /// `CP_Parity` derives a non-zero effective value from `CP_UartConfig`
    /// (ADR-071) when no explicit `CP_Parity` entry exists -- e.g.
    /// `CP_UartConfig = 8` (8E1) derives parity `2`. Round-tripping that
    /// derived value back through `SetComParam(PARITY, ...)` must not trip
    /// the guard (it is the same effective value Active already reports via
    /// the same derivation). The second assertion below is an end-to-end
    /// sanity check (an actual `CP_UartConfig` change still trips the
    /// guard) -- it does NOT isolate the `PARITY` derivation specifically,
    /// since `DATA_BITS` is itself compared independently in the same
    /// `BUSTYPE_UNUM32` iteration and would trip the guard on its own even
    /// with `PARITY` derivation removed entirely (confirmed by
    /// `edge-case-hunter` during PR #147 round 3 review, via a temporary
    /// repro that disabled just the derivation branch). See
    /// `bustype_params_differ_true_when_working_explicitly_disables_parity_derived_from_active_uart_config`
    /// below for the test that actually isolates the derivation logic.
    #[test]
    fn bustype_params_differ_false_on_derived_parity_roundtrip_but_true_on_actual_uart_config_change()
     {
        const UART_CONFIG_8E1: u32 = 8;
        assert_eq!(uart_config_to_parity(UART_CONFIG_8E1), Some(2));

        // Active: only CP_UartConfig=8E1 set (no explicit CP_Parity) --
        // GetComParam(PARITY) derives and reports 2.
        let mut active = ComParamSet::default();
        active
            .unum32
            .insert(ComParamId(j2534_0404::DATA_BITS), UART_CONFIG_8E1);

        // Working: the client round-tripped that derived value back as an
        // explicit CP_Parity=2 entry, alongside the same CP_UartConfig=8E1.
        let mut working = active.clone();
        working.unum32.insert(ComParamId(j2534_0404::PARITY), 2);
        assert!(
            !bustype_params_differ(&working, &active),
            "round-tripping CP_Parity's own ADR-071-derived effective value back as an explicit \
             entry must not be seen as a change"
        );

        // End-to-end sanity check (not a derivation isolator -- see doc
        // comment above): actually changing CP_UartConfig (still no
        // explicit CP_Parity) changes the derived effective parity (8N1 =>
        // 0), a real change that must still trip the guard.
        const UART_CONFIG_8N1: u32 = 6;
        assert_eq!(uart_config_to_parity(UART_CONFIG_8N1), Some(0));
        let mut working_changed = ComParamSet::default();
        working_changed
            .unum32
            .insert(ComParamId(j2534_0404::DATA_BITS), UART_CONFIG_8N1);
        assert!(
            bustype_params_differ(&working_changed, &active),
            "an actual CP_UartConfig change that changes the derived effective parity must still \
             be treated as a real BUSTYPE-class change"
        );
    }

    /// Isolates the `PARITY` derivation specifically (unlike the "actual
    /// `CP_UartConfig` change" case above, which also trips on `DATA_BITS`
    /// alone regardless of derivation correctness): `Working` explicitly
    /// disables UartConfig-implied parity (`CP_Parity = 0`) while `Active`
    /// has no explicit `CP_Parity` and derives a non-zero effective parity
    /// from its own `CP_UartConfig = 8` (8E1 => derived `2`, "even"). Naive
    /// presence/`unwrap_or(0)` comparison would see `working.PARITY = \
    /// Some(0)` vs. `active.PARITY.unwrap_or(0) = 0` and wrongly report "no
    /// change" -- missing a real attempted parity change (explicitly
    /// disabling UartConfig-implied parity IS a BUSTYPE-class change and
    /// must be rejected in a `temp_param_update` bracket). The
    /// derivation-aware `effective_unum32` compares `0` (explicit) against
    /// `2` (derived) and correctly trips.
    #[test]
    fn bustype_params_differ_true_when_working_explicitly_disables_parity_derived_from_active_uart_config()
     {
        const UART_CONFIG_8E1: u32 = 8;
        assert_eq!(uart_config_to_parity(UART_CONFIG_8E1), Some(2));

        let mut active = ComParamSet::default();
        active
            .unum32
            .insert(ComParamId(j2534_0404::DATA_BITS), UART_CONFIG_8E1);

        let mut working = active.clone();
        working.unum32.insert(ComParamId(j2534_0404::PARITY), 0);

        assert!(
            bustype_params_differ(&working, &active),
            "Working explicitly setting CP_Parity=0 while Active's CP_UartConfig derives a \
             non-zero effective parity is a real attempted BUSTYPE-class change and must trip \
             the guard, not be masked by a naive presence/unwrap_or(0) comparison"
        );
    }

    // ── tester_present_params_differ (ADR-133) ────────────────────────────────

    /// TESTER_PRESENT analog of
    /// `bustype_params_differ_false_on_unum32_zero_roundtrip_for_unseeded_param`:
    /// `PARAM_TESTER_PRESENT_HANDLING` unseeded, round-tripped as an explicit
    /// `0` via `SetComParam`, must not be seen as a change from Active's
    /// absent entry.
    #[test]
    fn tester_present_params_differ_false_on_unum32_zero_roundtrip_for_unseeded_param() {
        let mut working = ComParamSet::default();
        working.unum32.insert(PARAM_TESTER_PRESENT_HANDLING, 0);
        let active = ComParamSet::default();
        assert!(!tester_present_params_differ(&working, &active));
    }

    // ── apply_bustype_lock (ADR-110, corrected three-role design) ────────────

    #[test]
    fn apply_bustype_lock_not_locked_both_sets_pass_through_unchanged() {
        // Even a genuine Working/Active diff must pass through untouched
        // when nobody else holds the lock.
        let mut params = ComParamSet::default();
        params
            .unum32
            .insert(ComParamId(j2534_0404::DATA_RATE), 500_000);
        let mut active = ComParamSet::default();
        active
            .unum32
            .insert(ComParamId(j2534_0404::DATA_RATE), 250_000);

        let resolution = apply_bustype_lock(&params, &active, false);
        assert!(!resolution.rsc_locked);
        assert_eq!(resolution.hw_set, params);
        assert_eq!(resolution.promote_set, params);
    }

    #[test]
    fn apply_bustype_lock_locked_genuine_diff_excludes_from_hw_set_substitutes_promote_set_and_fires_event()
     {
        let mut params = ComParamSet::default();
        params
            .unum32
            .insert(ComParamId(j2534_0404::DATA_RATE), 500_000);
        let mut active = ComParamSet::default();
        active
            .unum32
            .insert(ComParamId(j2534_0404::DATA_RATE), 250_000);

        let resolution = apply_bustype_lock(&params, &active, true);
        assert!(resolution.rsc_locked);
        assert!(
            !resolution
                .hw_set
                .unum32
                .contains_key(&ComParamId(j2534_0404::DATA_RATE)),
            "a BUSTYPE key must never reach hardware while locked, regardless of own \
             Working-vs-Active agreement"
        );
        assert_eq!(
            resolution
                .promote_set
                .unum32
                .get(&ComParamId(j2534_0404::DATA_RATE)),
            Some(&250_000),
            "promote_set substitutes this CLL's own pre-call Active value"
        );
    }

    #[test]
    fn apply_bustype_lock_locked_genuine_diff_bytefield_excludes_and_substitutes() {
        let mut params = ComParamSet::default();
        params
            .bytes
            .insert(PARAM_CAN_BAUDRATE_RECORD, vec![1, 2, 3]);
        let mut active = ComParamSet::default();
        active
            .bytes
            .insert(PARAM_CAN_BAUDRATE_RECORD, vec![9, 9, 9]);

        let resolution = apply_bustype_lock(&params, &active, true);
        assert!(resolution.rsc_locked);
        assert!(
            !resolution
                .hw_set
                .bytes
                .contains_key(&PARAM_CAN_BAUDRATE_RECORD)
        );
        assert_eq!(
            resolution.promote_set.bytes.get(&PARAM_CAN_BAUDRATE_RECORD),
            Some(&vec![9, 9, 9])
        );
    }

    #[test]
    fn apply_bustype_lock_locked_no_bustype_diff_still_excludes_from_hw_set_but_no_event() {
        // This CLL's own Working already equals its own Active on the
        // BUSTYPE key (self-consistent) -- but that agreement can never
        // prove the value is safe against another CLL's real hardware
        // state, so it is still excluded from hw_set unconditionally.
        // Since this CLL attempted no change of its own, no event fires,
        // and promote_set for that key still ends up at the (unchanged)
        // old value.
        let mut params = ComParamSet::default();
        params
            .unum32
            .insert(ComParamId(j2534_0404::DATA_RATE), 500_000);
        let active = params.clone();

        let resolution = apply_bustype_lock(&params, &active, true);
        assert!(!resolution.rsc_locked);
        assert!(
            !resolution
                .hw_set
                .unum32
                .contains_key(&ComParamId(j2534_0404::DATA_RATE)),
            "hw_set exclusion is unconditional whenever locked, not gated on a diff"
        );
        assert_eq!(
            resolution
                .promote_set
                .unum32
                .get(&ComParamId(j2534_0404::DATA_RATE)),
            Some(&500_000)
        );
    }

    #[test]
    fn apply_bustype_lock_locked_key_absent_from_active_is_removed_from_promote_set() {
        let mut params = ComParamSet::default();
        params
            .unum32
            .insert(ComParamId(j2534_0404::DATA_RATE), 500_000);
        let active = ComParamSet::default();

        let resolution = apply_bustype_lock(&params, &active, true);
        assert!(resolution.rsc_locked);
        assert!(
            !resolution
                .hw_set
                .unum32
                .contains_key(&ComParamId(j2534_0404::DATA_RATE))
        );
        assert!(
            !resolution
                .promote_set
                .unum32
                .contains_key(&ComParamId(j2534_0404::DATA_RATE)),
            "absent from this CLL's own pre-call Active -> removed from promote_set too"
        );
    }

    /// Bytefield counterpart of
    /// `apply_bustype_lock_locked_key_absent_from_active_is_removed_from_promote_set`
    /// -- exercises `apply_bustype_lock`'s `BUSTYPE_BYTES` branch (the
    /// `None => promote_set.bytes.remove(&id)` arm) rather than the
    /// `BUSTYPE_UNUM32` one, per ADR-130's normalize-empty-to-absent fix
    /// making this reachable via a genuine (not just empty-round-tripped)
    /// `CoptUpdateparam` while another CLL holds the physical lock.
    #[test]
    fn apply_bustype_lock_locked_bytefield_key_absent_from_active_is_removed_from_promote_set() {
        let mut params = ComParamSet::default();
        params
            .bytes
            .insert(PARAM_CAN_BAUDRATE_RECORD, vec![1, 2, 3]);
        let active = ComParamSet::default();

        let resolution = apply_bustype_lock(&params, &active, true);
        assert!(resolution.rsc_locked);
        assert!(
            !resolution
                .hw_set
                .bytes
                .contains_key(&PARAM_CAN_BAUDRATE_RECORD)
        );
        assert!(
            !resolution
                .promote_set
                .bytes
                .contains_key(&PARAM_CAN_BAUDRATE_RECORD),
            "absent from this CLL's own pre-call Active -> removed from promote_set too"
        );
    }

    #[test]
    fn apply_bustype_lock_non_bustype_key_untouched_regardless_of_lock_state() {
        // CP_RequestAddrMode is not BUSTYPE class -- it must pass through
        // both hw_set and promote_set unchanged whether or not the lock is
        // held, and never contributes to rsc_locked.
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_REQUEST_ADDR_MODE, 2);
        let active = ComParamSet::default();

        for locked_by_other in [false, true] {
            let resolution = apply_bustype_lock(&params, &active, locked_by_other);
            assert!(!resolution.rsc_locked);
            assert_eq!(
                resolution.hw_set.unum32.get(&PARAM_REQUEST_ADDR_MODE),
                Some(&2)
            );
            assert_eq!(
                resolution.promote_set.unum32.get(&PARAM_REQUEST_ADDR_MODE),
                Some(&2)
            );
        }
    }

    #[test]
    fn apply_bustype_lock_locked_cp_parity_excluded_from_hw_set_like_any_other_bustype_key() {
        // Codex review finding on PR #116 (ADR-110 amendment): CP_Parity used
        // to be deliberately excluded from BUSTYPE_UNUM32, which let it
        // bypass this exclusion entirely -- a non-owning CLL could stage a
        // CP_Parity difference from Active and have it reach `hw_set`
        // unfiltered even while locked. Stage ONLY CP_Parity differing from
        // Active under locked_by_other=true and confirm it now behaves
        // exactly like any other BUSTYPE_UNUM32 key: excluded from hw_set,
        // substituted back to Active's value in promote_set, and correctly
        // trips `rsc_locked` since this CLL's own Working genuinely differs
        // from its own Active on this key.
        let mut params = ComParamSet::default();
        params.unum32.insert(ComParamId(j2534_0404::PARITY), 1);
        let mut active = ComParamSet::default();
        active.unum32.insert(ComParamId(j2534_0404::PARITY), 0);

        let resolution = apply_bustype_lock(&params, &active, true);
        assert!(
            resolution.rsc_locked,
            "this CLL's own Working (1) differs from its own Active (0) on CP_Parity, \
             so it did attempt a BUSTYPE change"
        );
        assert!(
            !resolution
                .hw_set
                .unum32
                .contains_key(&ComParamId(j2534_0404::PARITY)),
            "CP_Parity must never reach hardware while locked by another CLL, exactly \
             like every other BUSTYPE_UNUM32 key"
        );
        assert_eq!(
            resolution
                .promote_set
                .unum32
                .get(&ComParamId(j2534_0404::PARITY)),
            Some(&0),
            "promote_set substitutes this CLL's own pre-call Active value for CP_Parity"
        );
    }

    #[test]
    fn apply_bustype_lock_locked_node_address_excluded_from_hw_set_like_any_other_bustype_key() {
        // Codex review finding on PR #53 (mirrors the CP_Parity fix above,
        // ADR-110 amendment): PR #53 made NODE_ADDRESS client-writable via
        // SetComParam for J1850PWM/J1850VPW for the first time, but
        // NODE_ADDRESS was not in BUSTYPE_UNUM32 -- a non-owning CLL sharing
        // the same physical channel could stage a NODE_ADDRESS difference
        // from Active and have it reach `hw_set` unfiltered even while
        // another CLL held LOCK_PHYSICAL_COM_PARAMS. Stage ONLY NODE_ADDRESS
        // differing from Active under locked_by_other=true and confirm it
        // behaves exactly like any other BUSTYPE_UNUM32 key: excluded from
        // hw_set, substituted back to Active's value in promote_set, and
        // correctly trips `rsc_locked` since this CLL's own Working genuinely
        // differs from its own Active on this key.
        let mut params = ComParamSet::default();
        params
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0x33);
        let mut active = ComParamSet::default();
        active
            .unum32
            .insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);

        let resolution = apply_bustype_lock(&params, &active, true);
        assert!(
            resolution.rsc_locked,
            "this CLL's own Working (0x33) differs from its own Active (0xF1) on \
             NODE_ADDRESS, so it did attempt a BUSTYPE change"
        );
        assert!(
            !resolution
                .hw_set
                .unum32
                .contains_key(&ComParamId(j2534_0404::NODE_ADDRESS)),
            "NODE_ADDRESS must never reach hardware while locked by another CLL, exactly \
             like every other BUSTYPE_UNUM32 key"
        );
        assert_eq!(
            resolution
                .promote_set
                .unum32
                .get(&ComParamId(j2534_0404::NODE_ADDRESS)),
            Some(&0xF1),
            "promote_set substitutes this CLL's own pre-call Active value for NODE_ADDRESS"
        );
    }

    // ── strip_bustype_keys (ADR-110) ──────────────────────────────────────────

    #[test]
    fn strip_bustype_keys_removes_unum32_and_bytefield_bustype_keys() {
        let mut params = ComParamSet::default();
        params
            .unum32
            .insert(ComParamId(j2534_0404::DATA_RATE), 500_000);
        params
            .bytes
            .insert(PARAM_CAN_BAUDRATE_RECORD, vec![1, 2, 3]);
        params.unum32.insert(PARAM_REQUEST_ADDR_MODE, 2);

        let stripped = strip_bustype_keys(&params);
        assert!(
            !stripped
                .unum32
                .contains_key(&ComParamId(j2534_0404::DATA_RATE))
        );
        assert!(!stripped.bytes.contains_key(&PARAM_CAN_BAUDRATE_RECORD));
        assert_eq!(stripped.unum32.get(&PARAM_REQUEST_ADDR_MODE), Some(&2));
    }

    #[test]
    fn strip_bustype_keys_is_a_no_op_when_no_bustype_keys_are_present() {
        let mut params = ComParamSet::default();
        params.unum32.insert(PARAM_REQUEST_ADDR_MODE, 2);
        let stripped = strip_bustype_keys(&params);
        assert_eq!(stripped, params);
    }

    // ── strip_unchanged_analog_channel_wide_keys (ADR-216 Decision item 10
    // amendment; Codex review, PR #130, Finding 2) ────────────────────────────

    #[test]
    fn strip_unchanged_analog_channel_wide_keys_strips_a_value_unchanged_from_active() {
        let mut hw_set = ComParamSet::default();
        hw_set.unum32.insert(PARAM_ANALOG_AVERAGING_METHOD, 2);
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_ANALOG_AVERAGING_METHOD, 2);

        strip_unchanged_analog_channel_wide_keys(&mut hw_set, &active);

        assert!(
            !hw_set.unum32.contains_key(&PARAM_ANALOG_AVERAGING_METHOD),
            "a value this CLL never actually changed (staged == its own current Active) must \
             never be re-forwarded to hardware"
        );
    }

    #[test]
    fn strip_unchanged_analog_channel_wide_keys_retains_a_genuinely_changed_value() {
        let mut hw_set = ComParamSet::default();
        hw_set.unum32.insert(PARAM_ANALOG_AVERAGING_METHOD, 3);
        let mut active = ComParamSet::default();
        active.unum32.insert(PARAM_ANALOG_AVERAGING_METHOD, 2);

        strip_unchanged_analog_channel_wide_keys(&mut hw_set, &active);

        assert_eq!(
            hw_set.unum32.get(&PARAM_ANALOG_AVERAGING_METHOD),
            Some(&3),
            "a genuine change (staged differs from this CLL's own current Active) must always \
             forward -- these two params stay live-changeable via CoptUpdateparam (ADR-216 \
             Decision item 10)"
        );
    }

    #[test]
    fn strip_unchanged_analog_channel_wide_keys_strips_an_explicit_zero_matching_absent_active() {
        // hw_set carries an EXPLICIT 0 for CP_AnalogActiveChannels (e.g. a
        // client that staged the value straight back from a prior
        // GetComParam), while `active` has no entry at all -- the exact
        // shape a connect-time readback failure leaves behind (ADR-216
        // Context: "if the readback ever failed at connect time ... would
        // be absent (0 effective) in every CLL's Working"). effective_unum32
        // resolves BOTH sides to 0 (hw_set explicitly, active by the
        // documented absent-defaults-to-0 fallback), so this must strip --
        // not forward the explicit 0. This is the V3 scenario: without the
        // strip, this same forwarding would push CONFIG_ACTIVE_CHANNELS = 0
        // and, per clause 10.3.3.2.1's bitmask semantics, deactivate every
        // analog channel on the device.
        let mut hw_set = ComParamSet::default();
        hw_set.unum32.insert(PARAM_ANALOG_ACTIVE_CHANNELS, 0);
        let active = ComParamSet::default();

        strip_unchanged_analog_channel_wide_keys(&mut hw_set, &active);

        assert!(
            !hw_set.unum32.contains_key(&PARAM_ANALOG_ACTIVE_CHANNELS),
            "an explicit value equal to the effective (absent-defaults-to-0) Active must be \
             stripped, preventing an accidental CONFIG_ACTIVE_CHANNELS = 0 forward"
        );
    }

    fn access_timing_sf(entries: Vec<u32>) -> vci_service_interface::ParamStructfield {
        vci_service_interface::ParamStructfield {
            data: Some(
                vci_service_interface::param_structfield::Data::AccessTiming(
                    vci_service_interface::ParamAccessTimingList {
                        entries: vec![vci_service_interface::ParamAccessTiming {
                            p2_min: entries[0],
                            p2_max: entries[1],
                            p3_min: entries[2],
                            p3_max: entries[3],
                            p4_min: entries[4],
                            timing_set: entries.get(5).copied().unwrap_or(1),
                        }],
                    },
                ),
            ),
        }
    }

    fn session_timing_sf() -> vci_service_interface::ParamStructfield {
        vci_service_interface::ParamStructfield {
            data: Some(
                vci_service_interface::param_structfield::Data::SessionTiming(
                    vci_service_interface::ParamSessionTimingList {
                        entries: vec![vci_service_interface::ParamSessionTiming {
                            session: 1,
                            p2_max: 50,
                            p2_star: 100,
                        }],
                    },
                ),
            ),
        }
    }

    /// Codex review, PR #17, finding 2: `PARAM_ACCESS_TIMING_ECU` (and its
    /// AccessTiming-shaped siblings) must reject a `SessionTiming`-shaped
    /// value, not silently accept it -- storing one would later panic
    /// `store_access_timing_ecu_entry`'s `unreachable!()` on a qualifying
    /// ADR-146 Access Timing response.
    ///
    /// Fail-without-the-fix control: before this check existed,
    /// `validate_structfield_shape` (and `rpc_set_com_param`'s call to it)
    /// did not exist at all, so this call would have been a compile error --
    /// the equivalent pre-fix behavior (temporarily commenting out the
    /// `matches!` guard and returning `Ok(())` unconditionally for
    /// `Data::AccessTiming`/`Data::SessionTiming`) was confirmed to make
    /// this exact test fail before the guard was restored.
    #[test]
    fn access_timing_param_rejects_session_timing_shaped_value() {
        assert!(validate_structfield_shape(PARAM_ACCESS_TIMING_ECU, &session_timing_sf()).is_err());
        assert!(
            validate_structfield_shape(PARAM_ACCESS_TIMING_OVERRIDE, &session_timing_sf()).is_err()
        );
        assert!(validate_structfield_shape(PARAM_EXTENDED_TIMING, &session_timing_sf()).is_err());
    }

    /// The reverse mismatch: a `SessionTiming`-shaped param must reject an
    /// `AccessTiming`-shaped value.
    #[test]
    fn session_timing_param_rejects_access_timing_shaped_value() {
        let sf = access_timing_sf(vec![10, 20, 30, 40, 50]);
        assert!(validate_structfield_shape(PARAM_SESSION_TIMING_OVERRIDE, &sf).is_err());
        assert!(validate_structfield_shape(PARAM_SESSION_TIMING_ECU, &sf).is_err());
    }

    fn session_timing_sf_with(
        session: u32,
        p2_max: u32,
        p2_star: u32,
    ) -> vci_service_interface::ParamStructfield {
        vci_service_interface::ParamStructfield {
            data: Some(
                vci_service_interface::param_structfield::Data::SessionTiming(
                    vci_service_interface::ParamSessionTimingList {
                        entries: vec![vci_service_interface::ParamSessionTiming {
                            session,
                            p2_max,
                            p2_star,
                        }],
                    },
                ),
            ),
        }
    }

    /// Codex review, PR #22: `session`/`p2_max`/`p2_star` are `u32` proto
    /// fields, but ADR-150's consumers narrow them to `u16` with `as`,
    /// which wraps silently rather than rejecting -- a `session = 65539`
    /// override would alias session 3's entry, and a `p2_max = 65536`
    /// would alias to 0, corrupting a later derivation instead of being
    /// rejected up front here, at the same `rpc_set_com_param` validation
    /// point `AccessTiming`'s own field-range check already uses.
    ///
    /// Fail-without-the-fix control: reverting `validate_structfield_shape`'s
    /// `SessionTiming` arm to skip the per-field range loop (matching this
    /// test's pre-fix shape) was confirmed to make each of these three
    /// assertions fail before the loop was added.
    #[test]
    fn session_timing_param_rejects_fields_above_u16_range() {
        assert!(
            validate_structfield_shape(
                PARAM_SESSION_TIMING_OVERRIDE,
                &session_timing_sf_with(1, u32::from(u16::MAX) + 1, 100)
            )
            .is_err(),
            "p2_max above u16::MAX must be rejected, not silently truncated to 0 via `as u16`"
        );
        assert!(
            validate_structfield_shape(
                PARAM_SESSION_TIMING_ECU,
                &session_timing_sf_with(1, 100, u32::from(u16::MAX) + 1)
            )
            .is_err(),
            "p2_star above u16::MAX must be rejected, not silently truncated via `as u16`"
        );
        assert!(
            validate_structfield_shape(
                PARAM_SESSION_TIMING_OVERRIDE,
                &session_timing_sf_with(1, u32::from(u16::MAX), u32::from(u16::MAX))
            )
            .is_ok(),
            "u16::MAX itself is still in range for p2_max/p2_star and must be accepted"
        );
    }

    /// Codex review, PR #22 round 2: `session` is narrower than a bare
    /// `u16` -- only `1..=127` is ever producible by
    /// `SessionTimingConfig::with_request`'s `& 0x7F`-masked, zero-filtered
    /// capture (ISO 22900-2 section B.3.3.2.2's documented valid range). A value
    /// outside that range passed the round-1 `u16::MAX` check but could
    /// never match any real captured request -- silently dead input.
    ///
    /// Fail-without-the-fix control: reverting the `session` range check to
    /// the round-1 `> u16::MAX` check alone (matching this test's pre-fix
    /// shape) was confirmed to make both the `0` and `128` assertions fail
    /// before the narrower check was added.
    #[test]
    fn session_timing_param_rejects_session_outside_1_to_127() {
        assert!(
            validate_structfield_shape(
                PARAM_SESSION_TIMING_OVERRIDE,
                &session_timing_sf_with(0, 100, 100)
            )
            .is_err(),
            "session=0 is outside the valid range and must be rejected -- \
             with_request never captures it (filtered out explicitly)"
        );
        assert!(
            validate_structfield_shape(
                PARAM_SESSION_TIMING_ECU,
                &session_timing_sf_with(128, 100, 100)
            )
            .is_err(),
            "session=128 is outside the 7-bit valid range and must be rejected -- \
             with_request's `& 0x7F` mask can never produce it"
        );
        assert!(
            validate_structfield_shape(
                PARAM_SESSION_TIMING_OVERRIDE,
                &session_timing_sf_with(1, 100, 100)
            )
            .is_ok(),
            "session=1 (the range's lower bound) must be accepted"
        );
        assert!(
            validate_structfield_shape(
                PARAM_SESSION_TIMING_OVERRIDE,
                &session_timing_sf_with(127, 100, 100)
            )
            .is_ok(),
            "session=127 (the range's upper bound) must be accepted"
        );
    }

    /// A correctly-shaped value for the matching param_id is accepted.
    #[test]
    fn matching_shapes_are_accepted() {
        assert!(
            validate_structfield_shape(
                PARAM_ACCESS_TIMING_ECU,
                &access_timing_sf(vec![10, 20, 30, 40, 50])
            )
            .is_ok()
        );
        assert!(
            validate_structfield_shape(PARAM_SESSION_TIMING_OVERRIDE, &session_timing_sf()).is_ok()
        );
    }

    /// An explicitly empty Structfield (`data: None`) is valid for every
    /// STRUCTFIELD_PARAMS member -- there is nothing to mismatch.
    #[test]
    fn empty_structfield_is_always_accepted() {
        let empty = vci_service_interface::ParamStructfield { data: None };
        for param in STRUCTFIELD_PARAMS {
            assert!(validate_structfield_shape(*param, &empty).is_ok());
        }
    }

    /// Codex review, PR #17, finding 3: an AccessTiming field above 255
    /// (the ISO 14230-2 wire-byte range) must be rejected, not silently
    /// truncated via `as u8` wherever it is later read.
    ///
    /// Fail-without-the-fix control: temporarily removing the `value >
    /// u8::MAX as u32` check (letting every field through unconditionally)
    /// made this test fail before the check was restored.
    #[test]
    fn access_timing_field_above_255_is_rejected() {
        assert!(
            validate_structfield_shape(
                PARAM_ACCESS_TIMING_ECU,
                &access_timing_sf(vec![256, 20, 30, 40, 50])
            )
            .is_err(),
            "p2_min = 256 must be rejected"
        );
        assert!(
            validate_structfield_shape(
                PARAM_ACCESS_TIMING_ECU,
                &access_timing_sf(vec![10, 20, 30, 40, 50, 256])
            )
            .is_err(),
            "timing_set = 256 must be rejected too"
        );
        assert!(
            validate_structfield_shape(
                PARAM_ACCESS_TIMING_ECU,
                &access_timing_sf(vec![255, 255, 255, 255, 255])
            )
            .is_ok(),
            "255 (the max valid byte value) must be accepted"
        );
    }

    /// ADR-219 amendment (design-advisor decision): every `PARAM_*` constant
    /// `service_params.rs` mints must stay below the `0x0001_0000` vendor
    /// boundary `ComParamId::is_vendor()` checks -- the invariant that
    /// method's own doc comment depends on. If a future service-minted id
    /// ever crossed this boundary, `is_param_allowed`'s vendor early return
    /// would silently admit it on every protocol, bypassing that id's own
    /// protocol-specific exclusion.
    #[test]
    fn all_service_minted_comparam_ids_are_below_vendor_boundary() {
        let all_service_params: &[ComParamId] = &[
            PARAM_TESTER_PRESENT_MSG,
            PARAM_TESTER_PRESENT_INTERVAL_US,
            PARAM_TESTER_PRESENT_ADDR_MODE,
            PARAM_TESTER_PRESENT_EXP_POS_RESP,
            PARAM_TESTER_PRESENT_EXP_NEG_RESP,
            PARAM_TESTER_PRESENT_HANDLING,
            PARAM_TESTER_PRESENT_REQ_RSP,
            PARAM_TESTER_PRESENT_SEND_TYPE,
            PARAM_TESTER_PRESENT_TIME_ECU,
            PARAM_CYCLIC_RESP_TIMEOUT,
            PARAM_P2_STAR,
            PARAM_P2_STAR_ECU,
            PARAM_P2_MAX_ECU,
            PARAM_MODIFY_TIMING,
            PARAM_SESSION_TIMING_ECU,
            PARAM_SESSION_TIMING_OVERRIDE,
            PARAM_CAN_TRANSMISSION_TIME,
            PARAM_MESSAGE_INDICATION_RATE,
            PARAM_CHANGE_SPEED_TX_DELAY,
            PARAM_RC21_COMPLETION_TIMEOUT,
            PARAM_RC21_HANDLING,
            PARAM_RC21_REQUEST_TIME,
            PARAM_RC23_COMPLETION_TIMEOUT,
            PARAM_RC23_HANDLING,
            PARAM_RC23_REQUEST_TIME,
            PARAM_RC78_COMPLETION_TIMEOUT,
            PARAM_RC78_HANDLING,
            PARAM_RC_BYTE_OFFSET,
            PARAM_REPEAT_REQ_COUNT_APP,
            PARAM_SUSPEND_QUEUE_ON_ERROR,
            PARAM_CHANGE_SPEED_CTRL,
            PARAM_CHANGE_SPEED_MSG,
            PARAM_CHANGE_SPEED_RATE,
            PARAM_CHANGE_SPEED_RES_CTRL,
            PARAM_ENABLE_PERFORMANCE_TEST,
            PARAM_START_MSG_IND_ENABLE,
            PARAM_TRANSMIT_IND_ENABLE,
            PARAM_SW_CAN_HIGH_VOLTAGE,
            PARAM_N_AR,
            PARAM_N_AR_ECU,
            PARAM_N_AS,
            PARAM_N_AS_ECU,
            PARAM_N_BR,
            PARAM_N_BR_ECU,
            PARAM_N_BS,
            PARAM_N_BS_ECU,
            PARAM_N_CR,
            PARAM_N_CR_ECU,
            PARAM_N_CS,
            PARAM_N_CS_ECU,
            PARAM_ST_MIN_ECU,
            PARAM_BLOCK_SIZE_ECU,
            PARAM_ACCESS_TIMING_ECU,
            PARAM_ACCESS_TIMING_OVERRIDE,
            PARAM_EXTENDED_TIMING,
            PARAM_J1939_ADDR_CLAIM_TIMEOUT,
            PARAM_REPEAT_REQ_COUNT_TRANS,
            PARAM_CAN_PHYS_REQ_EXT_ADDR,
            PARAM_CAN_PHYS_REQ_FORMAT,
            PARAM_CAN_PHYS_REQ_ID,
            PARAM_CAN_RESP_USDT_EXT_ADDR,
            PARAM_CAN_RESP_USDT_FORMAT,
            PARAM_CAN_RESP_USDT_ID,
            PARAM_CAN_RESP_UUDT_EXT_ADDR,
            PARAM_CAN_RESP_UUDT_FORMAT,
            PARAM_CAN_RESP_UUDT_ID,
            PARAM_CAN_FUNC_REQ_EXT_ADDR,
            PARAM_CAN_FUNC_REQ_FORMAT,
            PARAM_CAN_FUNC_REQ_ID,
            PARAM_CAN_DATA_SIZE_OFFSET,
            PARAM_CAN_FILLER_BYTE,
            PARAM_CAN_FILLER_BYTE_HANDLING,
            PARAM_CAN_FIRST_CF_VALUE,
            PARAM_ECU_RESP_SOURCE_ADDR,
            PARAM_FUNC_REQ_FORMAT_PRIORITY,
            PARAM_FUNC_REQ_TARGET_ADDR,
            PARAM_FUNC_RESP_FORMAT_PRIORITY,
            PARAM_FUNC_RESP_TARGET_ADDR,
            PARAM_PHYS_REQ_FORMAT_PRIORITY,
            PARAM_PHYS_REQ_TARGET_ADDR,
            PARAM_PHYS_RESP_FORMAT_PRIORITY,
            PARAM_REQUEST_ADDR_MODE,
            PARAM_HEADER_FORMAT_J1850,
            PARAM_HEADER_FORMAT_KW,
            PARAM_ENABLE_CONCATENATION,
            PARAM_FILLER_BYTE,
            PARAM_FILLER_BYTE_HANDLING,
            PARAM_FILLER_BYTE_LENGTH,
            PARAM_5BAUD_ADDR_FUNC,
            PARAM_5BAUD_ADDR_PHYS,
            PARAM_SEND_REMOTE_FRAME,
            PARAM_TP_CONNECTION_MGMT,
            PARAM_MESSAGE_PRIORITY,
            PARAM_MID_REQ_ID,
            PARAM_MID_RESP_ID,
            PARAM_J1939_ADDR_NEG_RULE,
            PARAM_J1939_DATA_PAGE,
            PARAM_J1939_MAX_PACKET_TX,
            PARAM_J1939_PDU_FORMAT,
            PARAM_J1939_PDU_SPECIFIC,
            PARAM_J1939_SOURCE_ADDRESS,
            PARAM_J1939_TARGET_ADDRESS,
            PARAM_INIT_SETTINGS,
            PARAM_SCI_TRANSMIT_MODE,
            PARAM_J1939_PREFERRED_ADDRESS,
            PARAM_J1939_PREFERRED_ADDRESS_ECU,
            PARAM_J1939_NAME,
            PARAM_J1939_NAME_ECU,
            PARAM_J1939_SOURCE_NAME,
            PARAM_J1939_TARGET_NAME,
            PARAM_BIT_SAMPLE_POINT_ECU,
            PARAM_SAMPLES_PER_BIT,
            PARAM_SAMPLES_PER_BIT_ECU,
            PARAM_SYNC_JUMP_WIDTH_ECU,
            PARAM_LISTEN_ONLY,
            PARAM_CAN_BAUDRATE_RECORD,
            PARAM_K_L_LINE_INIT,
            PARAM_K_LINE_PULLUP,
            PARAM_TERMINATION_TYPE,
            PARAM_TERMINATION_TYPE_ECU,
            PARAM_CANFD_BAUDRATE,
            PARAM_CANFD_BIT_SAMPLE_POINT,
            PARAM_CANFD_SYNC_JUMP_WIDTH,
            PARAM_J1850_IFR_CTRL,
            PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK,
            PARAM_TEST_MODE,
            PARAM_ENABLE_INIT_SEQ_REPETITION,
            PARAM_TESTER_PRESENT_IMMED,
            PARAM_NUM_HEADER_BYTES_START_COMM_KW,
            PARAM_P3_FUNC,
            PARAM_P3_PHYS,
            PARAM_5BAUD_COMM_BAUDRATE_OVERRIDE,
            PARAM_5BAUD_INIT_BAUDRATE,
            PARAM_ISO_KEYBYTE_COUNT,
            PARAM_IGNORE_CHECKSUM,
            PARAM_CAN_MIXED_FORMAT,
            PARAM_CANFD_TX_MAX_DATA_LENGTH,
            PARAM_ESCAPE_SEQUENCE_HANDLING,
            PARAM_MAX_DATA_LENGTH_ECU,
            PARAM_MAX_CTS_REQ,
            PARAM_COLLISION_TEST_MODE,
            PARAM_SCI_SET_PROG_VOLTAGE,
            PARAM_SCI_ECU_SIMULATOR,
            PARAM_ANALOG_SAMPLE_RATE,
            PARAM_W1_MIN,
            PARAM_W2_MIN,
            PARAM_W3_MIN,
            PARAM_W4_MAX,
            PARAM_TP20_CHANNEL_SETUP_CAN_ID,
            PARAM_TP20_DESTINATION_ADDRESS,
            PARAM_TP20_TX_ID_PROPOSAL,
            PARAM_TP20_RX_ID_PROPOSAL,
            PARAM_TP20_APPLICATION_TYPE,
            PARAM_TP20_PASSIVE_IDENTIFIER,
            PARAM_TP20_PASSIVE_RX_ID,
            PARAM_TP20_BROADCAST_ADDRESS,
            PARAM_TP20_BROADCAST_INTERVAL,
            PARAM_NDIS_PIN_OPTION,
            PARAM_ANALOG_ACTIVE_CHANNELS,
            PARAM_ANALOG_SAMPLES_PER_READING,
            PARAM_ANALOG_READINGS_PER_MSG,
            PARAM_ANALOG_AVERAGING_METHOD,
            PARAM_ANALOG_SAMPLE_RESOLUTION,
            PARAM_ANALOG_INPUT_RANGE_LOW,
            PARAM_ANALOG_INPUT_RANGE_HIGH,
            PARAM_UEB_T0_MIN,
            PARAM_UEB_T1_MAX,
            PARAM_UEB_T2_MAX,
            PARAM_UEB_T3_MAX,
            PARAM_UEB_T4_MIN,
            PARAM_UEB_T5_MAX,
            PARAM_UEB_T6_MAX,
            PARAM_UEB_T7_MIN,
            PARAM_UEB_T7_MAX,
            PARAM_UEB_T9_MIN,
        ];
        for id in all_service_params {
            assert!(
                !id.is_vendor(),
                "service-minted ComParamId {:#010x} must stay below the 0x10000 vendor boundary",
                id.0
            );
        }
    }

    /// Boundary regression for the ADR-219 amendment: `0x0000_FFFF` (one
    /// below the vendor boundary) stays rejected on every protocol
    /// (unchanged), while `0x0001_0000` (the vendor boundary floor) is
    /// admitted on EVERY protocol -- including every closed-list one (Honda
    /// DIAG-H, Analog Inputs, Ethernet_NDIS, UART Echo Byte), not just CAN --
    /// proving `is_param_allowed`'s new vendor early return is checked ahead
    /// of every protocol-specific branch, closed lists included.
    #[test]
    fn vendor_boundary_is_admitted_on_every_protocol_including_closed_lists() {
        for protocol in [
            ChannelProtocol::CAN,
            ChannelProtocol::ISO15765,
            ChannelProtocol::ISO9141,
            ChannelProtocol::ISO14230,
            ChannelProtocol::J1850VPW,
            ChannelProtocol::J1850PWM,
            ChannelProtocol::SCI_A_ENGINE,
            ChannelProtocol::SCI_A_TRANS,
            ChannelProtocol::SCI_B_ENGINE,
            ChannelProtocol::SCI_B_TRANS,
            ChannelProtocol::HONDA_DIAGH_PS,
            ChannelProtocol::ANALOG_IN,
            ChannelProtocol::ETHERNET_NDIS,
            ChannelProtocol::UART_ECHO_BYTE_PS,
            ChannelProtocol::J1708_PS,
            ChannelProtocol::J1939_PS,
            ChannelProtocol::TP2_0_PS,
            // NOTE: `ChannelProtocol::GM_UART_PS` is deliberately excluded --
            // unlike every protocol above, `is_param_allowed` has no
            // dedicated branch for it at all, so it already falls through to
            // the generic "Unknown protocol — allow" fallback for any param
            // not otherwise excluded, including `0x0000_FFFF`; it would not
            // exercise this test's "closed-list protocol" claim.
        ] {
            assert!(
                check_param_allowed(protocol, ComParamId(0x0000_FFFF), None).is_err(),
                "0x0000_FFFF (one below the vendor boundary) should still be rejected for \
                 protocol {:#x}",
                protocol.value()
            );
            assert!(
                check_param_allowed(protocol, ComParamId(0x0001_0000), None).is_ok(),
                "0x0001_0000 (the vendor boundary) should be admitted unconditionally for \
                 protocol {:#x}",
                protocol.value()
            );
        }
    }
}
