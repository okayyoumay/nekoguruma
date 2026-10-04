/// Bus-type-specific default ComParam sets.
///
/// `bustype_default_params` returns the initial Working/Active `ComParamSet` for
/// a logical link created on a named physical bus type (e.g. "ISO_11898_2_DWCAN").
/// All values match the defaults in ISO 22900-3 Table B.1 (physical layer) and
/// are applied at `CreateComLogicalLink` time so clients can call `GetComParam`
/// without first issuing `SetComParam`.
///
/// Default values apply to both the Working and Active sets at creation time.
/// Callers that need different values should call `SetComParam` after creation.
use super::*;

/// Returns the default `ComParamSet` for the named bus type, or `None` when the
/// name is not recognized.  The lookup is case-insensitive.
pub(super) fn bustype_default_params(name: &str) -> Option<ComParamSet> {
    match name.to_ascii_lowercase().as_str() {
        // ── CAN family ───────────────────────────────────────────────────────
        "iso_11898_2_dwcan" | "iso-11898-2-dwcan" | "iso11898_2_dwcan" => Some(iso_11898_2_dwcan()),
        "iso_11898_3_dwftcan" | "iso-11898-3-dwftcan" | "iso11898_3_dwftcan" => {
            Some(iso_11898_3_dwftcan())
        }
        "sae_j1939_11_dwcan" | "sae-j1939-11-dwcan" | "j1939_11_dwcan" => {
            Some(sae_j1939_11_dwcan())
        }
        "sae_j2411_swcan" | "sae-j2411-swcan" | "j2411_swcan" => Some(sae_j2411_swcan()),
        // ── UART / K-Line family ─────────────────────────────────────────────
        "iso_14230_1_uart" | "iso-14230-1-uart" | "iso14230_1_uart" => Some(iso_14230_1_uart()),
        "iso_9141_2_uart" | "iso-9141-2-uart" | "iso9141_2_uart" => Some(iso_9141_2_uart()),
        "sae_j1708_uart" | "sae-j1708-uart" | "j1708_uart" => Some(sae_j1708_uart()),
        "sae_j2610_uart" | "sae-j2610-uart" | "j2610_uart" => Some(sae_j2610_uart()),
        // Combined K-line bus type (ISO 9141-2 UART and ISO 14230-1 UART on the
        // same connector): the resources table (ADR-069) connects this bus via
        // ISO9141, so it shares that bus type's physical-layer defaults.
        "iso_9141_2_uart_and_iso_14230_1_uart" => Some(iso_9141_2_uart()),
        // ── J1850 family ──────────────────────────────────────────────────────
        "sae_j1850_pwm" | "sae-j1850-pwm" | "j1850_pwm" | "j1850-pwm" => Some(sae_j1850_pwm()),
        "sae_j1850_vpw" | "sae-j1850-vpw" | "j1850_vpw" | "j1850-vpw" => Some(sae_j1850_vpw()),
        // Combined auto-detecting bus type (renamed from
        // `SAE_J1850_VPW_and_SAE_J1850_PWM`, ADR-070): seeded with the VPW
        // preset to match the VPW-first probe bias at
        // `ConnectComLogicalLink`; a PWM win overwrites the flavor-dependent
        // fields in place via `sae_j1850_pwm_override_params` instead of
        // changing this default.
        "sae_j1850" => Some(sae_j1850_vpw()),
        // SAE J2534-2 clause 12 UART Echo Byte Protocol (ADR-170/Phase 9):
        // this bus type has no ISO 22900-2 precedent to alias (ADR-170
        // Context), so this preset is this project's own choice, not a
        // Table B-derived one -- unlike every other entry in this function.
        "uart_echo_byte_uart" => Some(uart_echo_byte_uart()),
        // SAE J2534-2 clause 13 Honda DIAG-H Protocol (ADR-174/Phase 10):
        // same reasoning as UART Echo Byte just above -- this project's own
        // choice, no ISO 22900-2 precedent to alias. Unlike UART Echo
        // Byte's `DATA_RATE`, clause 13's baud rate is a fixed 9600bps that
        // is never `SetComParam`/`GetComParam`-reachable for this protocol
        // (`comparam_support::is_honda_diagh_param` excludes it) -- but this
        // preset still seeds `DATA_RATE` internally (see `honda_diagh_uart`'s
        // own doc comment) so `PassThruConnect`'s native baud-rate argument
        // is populated correctly; the value is unreachable via the
        // client-facing ComParam surface, not absent from the default set.
        "honda_diagh_uart" => Some(honda_diagh_uart()),
        // SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15, revised by
        // ADR-178): this bus type has no ISO 22900-2 precedent to alias (it
        // has no ISO 22900-2 source at all, `resources.rs`'s
        // `BUSTYPE_ANALOG_IN` doc comment). Unlike most other entries
        // above, this default set seeds exactly one ComParam --
        // `PARAM_ANALOG_SAMPLE_RATE`, the only param
        // `comparam_support::is_param_allowed`'s ANALOG_IN allowlist
        // accepts -- see `analog_in()`'s own doc comment for why this entry
        // still exists here even though its allowlist is nearly empty.
        "analog_in" => Some(analog_in()),
        // SAE J2534-2 clause 19 TP2.0 Protocol (ADR-188/Phase 7 Stage 7a):
        // this bus type has no ISO 22900-2 precedent to alias (ADR-188
        // Context), so this preset is this project's own choice, the same
        // "no Table B-derived precedent" shape UART Echo Byte's/Honda
        // DIAG-H's own entries above already document.
        "tp2_0_dwcan" | "tp2-0-dwcan" => Some(tp2_0_dwcan()),
        // SAE J2534-2 clause 11 GM UART Protocol (SAE J2740, ADR-189/Phase
        // 8): this bus type has no ISO 22900-2 precedent to alias, the same
        // "this project's own choice" shape UART Echo Byte's/Honda DIAG-H's/
        // TP2.0's own entries above already document -- but unlike those
        // three, clause 11's own Win32 API section defines no ComParam
        // concept at all (no baud rate, no loopback, nothing), so this
        // entry seeds no non-zero defaults; see `gm_uart_uart`'s own doc
        // comment.
        "gm_uart_uart" => Some(gm_uart_uart()),
        // SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): this bus
        // type's short name has a real ISO 22900-2:2022 Table B.2 anchor
        // (`IEEE_802_3`, DoIP's own physical-layer bustype name) -- the
        // first standalone-protocol bus type in this function with one,
        // unlike every "this project's own choice" entry above.
        "ieee_802_3" => Some(ieee_802_3()),
        _ => None,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Encodes a sequence of UNUM32 values as little-endian bytes for a Bytefield param.
fn encode_u32_seq(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

// ── CAN family defaults ───────────────────────────────────────────────────────

/// ISO 11898-2 DW-CAN (high-speed differential CAN, up to 1 Mbit/s).
/// Default: 500 kbit/s, sample point 80 %, SJW 15 %.
fn iso_11898_2_dwcan() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 500_000);
    p.unum32
        .insert(ComParamId(j2534_0404::BIT_SAMPLE_POINT), 80);
    p.unum32.insert(PARAM_LISTEN_ONLY, 0);
    p.unum32.insert(PARAM_SAMPLES_PER_BIT, 0);
    p.unum32.insert(ComParamId(j2534_0404::SYNC_JUMP_WIDTH), 15);
    p.unum32.insert(PARAM_TERMINATION_TYPE, 0);
    p.unum32.insert(PARAM_CANFD_BAUDRATE, 0);
    p.unum32.insert(PARAM_CANFD_BIT_SAMPLE_POINT, 80);
    p.unum32.insert(PARAM_CANFD_SYNC_JUMP_WIDTH, 15);
    // CP_CanBaudrateRecord: "12 2 500000 250000" — strip (max_count actual_count), store values only
    p.bytes.insert(
        PARAM_CAN_BAUDRATE_RECORD,
        encode_u32_seq(&[500_000, 250_000]),
    );
    p
}

/// ISO 11898-3 DW-FT-CAN (fault-tolerant differential CAN, up to 125 kbit/s).
/// Default: 125 kbit/s (Table B.21 -- CP_Baudrate, `ISO_11898_3_DWFTCAN = 125k`;
/// ADR-130 corrects this from a stale 500 kbit/s that exceeded the bus's own
/// physical limit).
///
/// Table B.21's CP_CanBaudrateRecord row defines a default only for
/// `ISO_11898_2_DWCAN` and `SAE_J1939_11_DWCAN` (the two bus types that use
/// this record for OBD CAN-speed auto-detection); FT-CAN has none, so this
/// preset does not populate one (see ADR-130).
fn iso_11898_3_dwftcan() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 125_000);
    p.unum32
        .insert(ComParamId(j2534_0404::BIT_SAMPLE_POINT), 80);
    p.unum32.insert(PARAM_LISTEN_ONLY, 0);
    p.unum32.insert(PARAM_SAMPLES_PER_BIT, 0);
    p.unum32.insert(ComParamId(j2534_0404::SYNC_JUMP_WIDTH), 15);
    p.unum32.insert(PARAM_TERMINATION_TYPE, 0);
    p
}

/// SAE J1939 on 11-bit DW-CAN (250 kbit/s standard rate).
fn sae_j1939_11_dwcan() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 250_000);
    p.unum32
        .insert(ComParamId(j2534_0404::BIT_SAMPLE_POINT), 80);
    p.unum32.insert(PARAM_LISTEN_ONLY, 0);
    p.unum32.insert(PARAM_SAMPLES_PER_BIT, 0);
    p.unum32.insert(ComParamId(j2534_0404::SYNC_JUMP_WIDTH), 15);
    p.unum32.insert(PARAM_TERMINATION_TYPE, 0);
    p.unum32.insert(PARAM_CANFD_BAUDRATE, 0);
    p.unum32.insert(PARAM_CANFD_BIT_SAMPLE_POINT, 80);
    p.unum32.insert(PARAM_CANFD_SYNC_JUMP_WIDTH, 15);
    // CP_CanBaudrateRecord: "12 1 250000" — strip (max_count actual_count), store values only
    p.bytes
        .insert(PARAM_CAN_BAUDRATE_RECORD, encode_u32_seq(&[250_000]));
    p
}

/// SAE J2411 SW-CAN (Single Wire CAN, 33.333 kbit/s normal mode).
fn sae_j2411_swcan() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 33_333);
    p.unum32
        .insert(ComParamId(j2534_0404::BIT_SAMPLE_POINT), 87);
    p.unum32.insert(PARAM_LISTEN_ONLY, 0);
    p.unum32.insert(PARAM_SAMPLES_PER_BIT, 0);
    p.unum32.insert(ComParamId(j2534_0404::SYNC_JUMP_WIDTH), 15);
    p.unum32.insert(PARAM_TERMINATION_TYPE, 0);
    p
}

// ── UART / K-Line family defaults ─────────────────────────────────────────────

/// ISO 14230-1 UART (KWP2000, K/L-line, 10.4 kbit/s).
fn iso_14230_1_uart() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 10_400);
    p.unum32.insert(PARAM_K_L_LINE_INIT, 0);
    p.unum32.insert(PARAM_K_LINE_PULLUP, 0);
    p.unum32.insert(ComParamId(j2534_0404::DATA_BITS), 6);
    p
}

/// ISO 9141-2 UART (K/L-line, 10.4 kbit/s).
fn iso_9141_2_uart() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 10_400);
    p.unum32.insert(PARAM_K_L_LINE_INIT, 0);
    p.unum32.insert(PARAM_K_LINE_PULLUP, 0);
    p.unum32.insert(ComParamId(j2534_0404::DATA_BITS), 6);
    p
}

/// SAE J1708 UART (heavy-duty truck serial bus, 9.6 kbit/s).
fn sae_j1708_uart() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 9_600);
    p.unum32.insert(ComParamId(j2534_0404::DATA_BITS), 6);
    // Codex review finding, PR #64: clause 17.4.5's own text gives
    // CP_MessagePriority a default of 8 (the lowest priority) -- seeded here
    // so a fresh J1708 link's GetComParam(CP_MessagePriority) reports the
    // same value `ComParamSet::msg_priority_tx_flags`'s absent/0/
    // out-of-range clamp already produces on the wire, rather than the
    // generic unset 0.
    p.unum32.insert(PARAM_MESSAGE_PRIORITY, 8);
    p
}

/// SAE J2610 UART / SCI (Chrysler Single-Wire, 7.8125 kbit/s).
fn sae_j2610_uart() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 7_812);
    p.unum32.insert(ComParamId(j2534_0404::DATA_BITS), 6);
    p
}

/// SAE J2534-2 clause 12 UART Echo Byte Protocol (K-line, ADR-170/Phase 9):
/// clause 12.3.4.1's own `DATA_RATE` default is 9600 bps.
fn uart_echo_byte_uart() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 9_600);
    // SAE J2534-2 clause 12.3.4.1 UART Echo Byte timing ComParams (ADR-216):
    // project-invented, no ISO 22900-2 source, so these are this project's
    // own choice too -- seeded per SAE J2534-2 Table 36's own per-parameter
    // defaults, native-verbatim whole milliseconds (not the ISO 1 us `CP_*`
    // convention, ADR-216 Decision item 2).
    p.unum32.insert(PARAM_UEB_T0_MIN, 10);
    p.unum32.insert(PARAM_UEB_T1_MAX, 400);
    p.unum32.insert(PARAM_UEB_T2_MAX, 200);
    p.unum32.insert(PARAM_UEB_T3_MAX, 200);
    p.unum32.insert(PARAM_UEB_T4_MIN, 1);
    p.unum32.insert(PARAM_UEB_T5_MAX, 1_000);
    p.unum32.insert(PARAM_UEB_T6_MAX, 200);
    p.unum32.insert(PARAM_UEB_T7_MIN, 1);
    p.unum32.insert(PARAM_UEB_T7_MAX, 40);
    p.unum32.insert(PARAM_UEB_T9_MIN, 1);
    p
}

/// SAE J2534-2 clause 13 Honda DIAG-H Protocol (K-line, ADR-174/Phase 10):
/// seeds an internal-only `DATA_RATE` of 9600 bps (clause 13.3.1's fixed
/// baud rate) -- `ComParamSet::baud_rate()` returns `0` when `DATA_RATE` is
/// absent, and `rpc_connect_com_logical_link` forwards that value directly
/// to native `PassThruConnect`, so omitting this entry would connect every
/// Honda DIAG-H link at baud rate 0 instead of clause 13's mandatory 9600
/// (Codex review finding, PR #63). This is deliberately still excluded from
/// `comparam_support::is_honda_diagh_param`'s allowlist, so it stays
/// unreachable via client-facing `SetComParam`/`GetComParam` (matching
/// clause 13.3.1's "not configurable" contract) -- the mismatch between
/// "seeded for internal connect use" and "not client-facing" is deliberate,
/// not a contradiction: the value backs the native connect call, not a
/// D-PDU ComParam surface. Clause 13.3.1 also states this protocol reuses
/// the existing SAE J2534-1 `ISO9141` timing parameters rather than
/// defining its own -- `P1_MAX`/`P3_MIN`/`P4_MIN` below match
/// `kwp_on_9141_common`'s own ISO9141 K-line defaults exactly
/// (20ms/55ms/5ms, all in microseconds per this codebase's existing
/// convention).
fn honda_diagh_uart() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 9_600);
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(ComParamId(j2534_0404::P1_MAX), 20_000);
    p.unum32.insert(ComParamId(j2534_0404::P3_MIN), 55_000);
    p.unum32.insert(ComParamId(j2534_0404::P4_MIN), 5_000);
    p
}

/// SAE J2534-2 clause 10 Analog Inputs (ADR-177/Phase 15; revised by
/// ADR-178): clause 10 defines no baud rate, loopback, or any other native
/// ComParam-shaped concept, so this set seeds exactly one entry --
/// `PARAM_ANALOG_SAMPLE_RATE` (the project-minted `CP_AnalogSampleRate`,
/// ADR-178) -- at its unset/unstaged default of `0`. A caller must
/// `SetComParam` it nonzero before `ConnectComLogicalLink`; clause
/// 10.3.3.2.2's own zero default disables the acquisition subsystem, so
/// `rpc_connect_com_logical_link` rejects a still-zero value at connect
/// time for an Analog Input resource (see the allowlist gain in
/// `comparam_support.rs`'s `ANALOG_IN` branch that makes this ComParam
/// settable at all). See `bustype_default_params`'s own `"analog_in"`
/// match arm for why this function exists.
fn analog_in() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_ANALOG_SAMPLE_RATE, 0);
    // SAE J2534-2 clause 10 Analog Inputs remaining parameters (ADR-216):
    // `CP_AnalogSamplesPerReading`/`CP_AnalogReadingsPerMsg`/
    // `CP_AnalogAveragingMethod` seed Table 16's own per-parameter defaults.
    // `CP_AnalogActiveChannels` is deliberately NOT seeded here -- clause
    // 10.3.3.2.1's own default is device-dependent, so it stays absent
    // (reads `0`, "not yet known") until the connect-time GET_CONFIG
    // readback (ADR-216 Decision item 6) fills it in. The three read-only
    // capability ComParams (`CP_AnalogSampleResolution`/
    // `CP_AnalogInputRangeLow`/`CP_AnalogInputRangeHigh`) get no static
    // default for the same reason -- readback-only.
    p.unum32.insert(PARAM_ANALOG_SAMPLES_PER_READING, 1);
    p.unum32.insert(PARAM_ANALOG_READINGS_PER_MSG, 1);
    p.unum32.insert(PARAM_ANALOG_AVERAGING_METHOD, 0);
    p
}

/// SAE J2534-2 clause 19 TP2.0 Protocol on DW-CAN (ADR-188/Phase 7 Stage
/// 7a; extended by ADR-192/Phase 7 Stage 7c): clause 19.3.1's fixed 500
/// kbit/s requirement -- unlike Honda DIAG-H's own fixed-baud-rate
/// exclusion, `DATA_RATE` stays client-settable for TP2.0 (Table 77 lists it
/// as a supported override, `comparam_support::is_tp20_param`).
/// `BIT_SAMPLE_POINT`/`SYNC_JUMP_WIDTH` default to the same 80%/15% every
/// other DW-CAN preset in this file uses. The five minted `PARAM_TP20_*`
/// ComParams (setup CAN ID, destination address, TX-ID/RX-ID proposal,
/// application type) are deliberately left unset -- no spec-mandated default
/// exists for them (ADR-188 Decision item 4); a client must stage all five
/// via `SetComParam` before `CoptStartcomm`. Stage 7b's two passive-connection
/// ComParams (`PARAM_TP20_PASSIVE_IDENTIFIER`/`_RX_ID`) and Stage 7c's
/// `PARAM_TP20_BROADCAST_ADDRESS` are likewise left unset for the same
/// reason ("no broadcast"/"no passive arm" by omission is the correct
/// default, ADR-192 Decision item 1). `PARAM_TP20_BROADCAST_INTERVAL` is the
/// one exception: it seeds to `20`, Table 77's own spec-mandated
/// `T_BR_INT` default (ADR-192 Decision item 3).
fn tp2_0_dwcan() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 500_000);
    p.unum32
        .insert(ComParamId(j2534_0404::BIT_SAMPLE_POINT), 80);
    p.unum32.insert(ComParamId(j2534_0404::SYNC_JUMP_WIDTH), 15);
    p.unum32.insert(PARAM_TP20_BROADCAST_INTERVAL, 20);
    p
}

/// SAE J2534-2 clause 11 GM UART Protocol (SAE J2740, ADR-189/Phase 8):
/// unlike every other standalone-protocol entry in this file (UART Echo
/// Byte/Honda DIAG-H/TP2.0), clause 11's own Win32 API section (11.3)
/// defines no ComParam concept whatsoever -- no baud rate, no loopback, no
/// timing parameters. `DATA_RATE` (the universal param every protocol
/// accepts, `comparam_support::is_universal_param`) is seeded at its
/// unset/unstaged default of `0` -- the same "no spec-mandated default
/// exists, a caller must `SetComParam` it before connecting" shape
/// `analog_in`'s own `PARAM_ANALOG_SAMPLE_RATE` entry documents -- rather
/// than a numeric value this project would otherwise have to invent with no
/// textual basis. `GM_UART_PS` has no dedicated `comparam_support::
/// is_param_allowed` branch (ADR-189 leaves this deliberately unscoped, a
/// residual for a future phase), so it falls through to that function's
/// generic "Unknown protocol — allow" tail -- `DATA_RATE` and every other
/// ComParam stay `SetComParam`/`GetComParam`-reachable on a GM UART link
/// regardless of this entry's own zero seed.
fn gm_uart_uart() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 0);
    p
}

/// SAE J2534-2 clause 24 Ethernet_NDIS (ADR-194/Phase 16): clause 24 defines
/// no baud rate, loopback, or any other native ComParam-shaped concept, so
/// this set seeds exactly one entry -- `PARAM_NDIS_PIN_OPTION` (the
/// project-minted `CP_NdisPinOption`) -- at its default of `0` (auto: neither
/// `CONNECT_FLAG_NDIS_PINS_OPTION1`/`OPTION2` bit set). Unlike
/// `PARAM_ANALOG_SAMPLE_RATE`'s own zero-disables-the-subsystem default
/// (ADR-177/ADR-178), `0` here is spec-functional -- a caller need not
/// `SetComParam` this before connecting (ADR-194 Decision).
fn ieee_802_3() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_NDIS_PIN_OPTION, 0);
    p
}

// ── J1850 family defaults ─────────────────────────────────────────────────────

/// SAE J1850 PWM (41.6 kbit/s pulse-width modulation).
fn sae_j1850_pwm() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 41_600);
    p.unum32.insert(ComParamId(j2534_0404::NETWORK_LINE), 0);
    p.unum32.insert(PARAM_J1850_IFR_CTRL, 1);
    p
}

/// SAE J1850 VPW (10.4 kbit/s variable pulse width).
fn sae_j1850_vpw() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::DATA_RATE), 10_400);
    p.unum32.insert(PARAM_J1850_IFR_CTRL, 0);
    p
}

/// Returns the *flavor-dependent subset* of the merged bus-then-protocol
/// "PWM" `ComParamSet` for a `SAE_J1850` bus-agnostic `protocol`
/// (`SAE_J2190_ON_SAE_J1850` / `ISO_15031_5_ON_SAE_J1850`): only the keys
/// whose value actually differs between the merged VPW preset (the one
/// `CreateComLogicalLink` seeds the Working set with, per the VPW-first bias)
/// and the merged PWM preset (bus defaults first, then protocol defaults on
/// top, same layering `CreateComLogicalLink` uses).
///
/// Used by the `SAE_J1850` VPW/PWM auto-detect probe (ADR-070) at
/// `ConnectComLogicalLink`: on a PWM win, the caller extends the CLL's
/// Working `ComParamSet` with this result in place. Restricting the result
/// to the diff (verification-pass fix) means a client's `SetComParam` on any
/// flavor-independent key (e.g. `CP_P2Max`, `CP_FuncReqTargetAddr` -- staged
/// before `ConnectComLogicalLink`, per ADR-067/ADR-028) survives the
/// override: `DATA_RATE`/`NETWORK_LINE`/`CP_J1850IFRCtrl` from the bus preset
/// and the header/priority-byte fields from the protocol preset -- for both
/// `SAE_J2190_on_SAE_J1850` and `ISO_15031_5_on_SAE_J1850`, whose PWM/VPW
/// `CP_FuncReqFormatPriorityType`/`CP_PhysReqFormatPriorityType`/
/// `CP_FuncRespFormatPriorityType`/`CP_PhysRespFormatPriorityType` header
/// bytes genuinely differ (a second verification-pass fix corrected
/// `iso_15031_5_on_sae_j1850_pwm`, which previously carried the VPW byte
/// values verbatim -- a diff against the *correct* PWM preset is what makes
/// this key part of the override at all; a wrong-but-identical-looking PWM
/// preset would have hidden the bug behind this same diff mechanism) --
/// these still win with the detected flavor's value regardless of client
/// staging, since they are documented as auto-managed on this bus, but
/// every other key (identical in both presets) is no longer touched, where
/// the full preset previously clobbered it unconditionally.
///
/// Returns `None` for any other protocol.
pub(super) fn sae_j1850_pwm_override_params(protocol: ChannelProtocol) -> Option<ComParamSet> {
    let (proto_pwm, proto_vpw) = match protocol {
        ChannelProtocol::SAE_J2190_ON_SAE_J1850 => {
            (sae_j2190_on_sae_j1850_pwm(), sae_j2190_on_sae_j1850_vpw())
        }
        ChannelProtocol::ISO_15031_5_ON_SAE_J1850 => (
            iso_15031_5_on_sae_j1850_pwm(),
            iso_15031_5_on_sae_j1850_vpw(),
        ),
        _ => return None,
    };

    let mut pwm = sae_j1850_pwm();
    pwm.unum32.extend(proto_pwm.unum32);
    pwm.bytes.extend(proto_pwm.bytes);
    pwm.structfield.extend(proto_pwm.structfield);

    let mut vpw = sae_j1850_vpw();
    vpw.unum32.extend(proto_vpw.unum32);
    vpw.bytes.extend(proto_vpw.bytes);
    vpw.structfield.extend(proto_vpw.structfield);

    Some(comparamset_diff(&pwm, &vpw))
}

/// Returns the subset of `pwm`'s entries whose value differs from (or is
/// absent in) `vpw` -- the flavor-dependent keys
/// `sae_j1850_pwm_override_params` applies on a PWM win. A key present in
/// both sets with the identical value is omitted, so re-applying it as an
/// "override" would be a no-op anyway -- omitting it is what lets a
/// same-key client-staged Working value survive instead of being
/// unconditionally overwritten.
fn comparamset_diff(pwm: &ComParamSet, vpw: &ComParamSet) -> ComParamSet {
    let mut out = ComParamSet::default();
    for (&id, value) in &pwm.unum32 {
        if vpw.unum32.get(&id) != Some(value) {
            out.unum32.insert(id, *value);
        }
    }
    for (&id, value) in &pwm.bytes {
        if vpw.bytes.get(&id) != Some(value) {
            out.bytes.insert(id, value.clone());
        }
    }
    for (&id, value) in &pwm.structfield {
        if vpw.structfield.get(&id) != Some(value) {
            out.structfield.insert(id, value.clone());
        }
    }
    out
}

// ── Protocol-name-specific default ComParam sets ─────────────────────────────

/// Resource-table `protocol_name`s with **intentionally** no
/// `protocol_default_params` entry, checked exhaustively against the whole
/// resources table by
/// `resources::tests::every_resource_row_has_protocol_and_bustype_defaults_or_is_allowlisted`
/// (ADR-069).
///
/// Each of these five is a *native* `ChannelProtocol` (no application-layer
/// overlay: `ISO15765`/`ISO9141`/`ISO14230`/`J1850PWM`/`J1850VPW`) whose
/// resource-table name is actually an ISO 22900-2 BUSTYPE name
/// (`bustype_default_params` already covers it: `iso_9141_2_uart`,
/// `iso_14230_1_uart`, `sae_j1850_pwm`, `sae_j1850_vpw`), not one of the 21
/// ISO 22900-3 Table B protocol-name entries this function implements.
/// `ISO_11898_RAW` is the one raw-layer resource that *is* a genuine Table B
/// entry (`iso_11898_raw`, with its own P2/P3 timing) — it is deliberately
/// NOT in this list, precisely because it has a real preset.
///
/// **Correction (2026-07-28, PR #7):** "ISO 22900-3" above is wrong — no such
/// document exists in this workspace, and the boundary this list draws is
/// ISO 22900-2:2009(E) Annex B's, not a different document's. Four of these
/// five names (`ISO_9141_2`, `ISO_14230_4`, `SAE_J1850_VPW`, `SAE_J1850_PWM`)
/// do appear as column headers in Annex B's Tables B.10/B.19 — but those
/// transport-named columns denote the `ISO_15031_5` OBD application stack
/// keyed by its transport (`ISO_15031_5` itself has no column; its values
/// vary per transport and live in these columns instead — confirmed by the
/// `ISO_9141_2` column defaulting tester-present *on* with the OBD $01/$00
/// keep-alive, which would be nonsensical on a bare, application-layer-free
/// channel). The bare native protocols correctly have no B.19-derived
/// defaults; this list's Decision (allowlist, no protocol-layer overlay) was
/// always correct, only the stated reason was wrong. See ADR-069's own
/// matching correction for the full derivation.
///
/// `#[cfg(test)]`-only: nothing at runtime consults this list (an
/// unrecognized `protocol_name` is simply skipped -- no protocol-layer
/// overrides applied on top of the bus-type defaults, which is exactly the
/// intended behavior for these five), so it exists solely for
/// `resources::tests`'s exhaustiveness check.
#[cfg(test)]
pub(super) const PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS: &[&str] = &[
    "ISO_15765_2", // resource 0x0206 -- native ISO15765; see ISO_11898_2_DWCAN bustype default
    "ISO_9141_2",  // resource 0x0210 -- native ISO9141; see ISO_9141_2_UART bustype default
    "ISO_14230_4", // resource 0x020C -- native ISO14230; see ISO_14230_1_UART bustype default
    "SAE_J1850_PWM", // resource 0x0216 -- native J1850PWM; see SAE_J1850_PWM bustype default
    "SAE_J1850_VPW", // resource 0x0219 -- native J1850VPW; see SAE_J1850_VPW bustype default
    // ADR-164/Phase 4: resource 0x022B -- the SWCAN sibling of native
    // ISO15765 resource 0x0206 above; see SAE_J2411_SWCAN bustype default.
    "ISO_15765_2_SWCAN",
    // ADR-168/Phase 6: resource 0x0235 -- the FTCAN sibling of native
    // ISO15765 resource 0x0206 above; see ISO_11898_3_DWFTCAN bustype default.
    "ISO_15765_2_FTCAN",
    // ADR-170/Phase 9: resource 0x023A -- native UART_ECHO_BYTE_PS, whose
    // resource-table name is the same project-chosen name as its own bus
    // type; see UART_ECHO_BYTE_UART bustype default above.
    "UART_ECHO_BYTE",
    // ADR-174/Phase 10: resource 0x023B -- native HONDA_DIAGH_PS, whose
    // resource-table name is the same project-chosen name as its own bus
    // type; see HONDA_DIAGH_UART bustype default above.
    "HONDA_DIAGH",
    // ADR-175/Phase 11: resource 0x023C -- native J1708_PS, whose
    // resource-table name (`SAE_J1708`) is a project-chosen name distinct
    // from its own bus type's name (`SAE_J1708_UART`, `sae_j1708_uart()`
    // bustype default above) -- clause 17 defines no protocol-layer overlay
    // for this bare protocol.
    "SAE_J1708",
    // ADR-177/Phase 15 (revised by ADR-178): resources 0x023D-0x025C -- the
    // 32 native PROTOCOL_ANALOG_IN_x resources, each named after its own
    // native id ("ANALOG_IN_1".."ANALOG_IN_32"); see the "analog_in"
    // bustype default above (seeds `PARAM_ANALOG_SAMPLE_RATE`, the one
    // ComParam this protocol's allowlist accepts -- not empty, but no
    // separate protocol-layer overlay beyond that one seed was omitted).
    "ANALOG_IN_1",
    "ANALOG_IN_2",
    "ANALOG_IN_3",
    "ANALOG_IN_4",
    "ANALOG_IN_5",
    "ANALOG_IN_6",
    "ANALOG_IN_7",
    "ANALOG_IN_8",
    "ANALOG_IN_9",
    "ANALOG_IN_10",
    "ANALOG_IN_11",
    "ANALOG_IN_12",
    "ANALOG_IN_13",
    "ANALOG_IN_14",
    "ANALOG_IN_15",
    "ANALOG_IN_16",
    "ANALOG_IN_17",
    "ANALOG_IN_18",
    "ANALOG_IN_19",
    "ANALOG_IN_20",
    "ANALOG_IN_21",
    "ANALOG_IN_22",
    "ANALOG_IN_23",
    "ANALOG_IN_24",
    "ANALOG_IN_25",
    "ANALOG_IN_26",
    "ANALOG_IN_27",
    "ANALOG_IN_28",
    "ANALOG_IN_29",
    "ANALOG_IN_30",
    "ANALOG_IN_31",
    "ANALOG_IN_32",
    // ADR-188/Phase 7 Stage 7a: resource 0x025F -- native TP2_0_PS, whose
    // resource-table name (`SAE_J2819_TP2_0`) is a project-chosen name
    // distinct from its own bus type's name (`TP2_0_DWCAN`, `tp2_0_dwcan()`
    // bustype default above) -- clause 19 defines no protocol-layer overlay
    // for this bare protocol, the same J1708 reasoning above.
    "SAE_J2819_TP2_0",
    // ADR-189/Phase 8: resource 0x0260 -- native GM_UART_PS, whose
    // resource-table name (`GM_UART`) is a project-chosen name distinct
    // from its own bus type's name (`GM_UART_UART`, `gm_uart_uart()`
    // bustype default above) -- clause 11 defines no protocol-layer overlay
    // for this bare protocol, the same J1708/TP2.0 reasoning above.
    "GM_UART",
    // ADR-194/Phase 16: resource 0x0261 -- native ETHERNET_NDIS, whose
    // resource-table name (`ETHERNET_NDIS`) is a project-chosen name
    // distinct from its own bus type's name (`IEEE_802_3`, `ieee_802_3()`
    // bustype default above) -- clause 24 defines no protocol-layer overlay
    // for this bare protocol, the same J1708/TP2.0/GM UART reasoning above.
    "ETHERNET_NDIS",
];

/// Returns the default `ComParamSet` for the named D-PDU protocol, or `None`
/// when the name is not recognized.  The lookup is case-insensitive.
///
/// These defaults are applied **on top of** bus-type defaults at
/// `CreateComLogicalLink` time, overriding bus-type values where they overlap.
pub(super) fn protocol_default_params(name: &str) -> Option<ComParamSet> {
    let p = match name.to_ascii_lowercase().as_str() {
        "iso_11898_raw" => Some(iso_11898_raw()),
        "iso_14230_3_on_iso_14230_2" => Some(iso_14230_3_on_iso_14230_2()),
        "iso_14230_3_on_iso_15765_2" => Some(iso_14230_3_on_iso_15765_2()),
        // ISO_14229_3 supersedes ISO_15765_3 and offers the same feature
        // set (ADR-069, resource 0x0202); ISO_14229_3_on_ISO_15765_2
        // is spec-identical to ISO_15765_3_on_ISO_15765_2 (resource 0x0203).
        // Both therefore share that preset's UDS/ISO-TP timing, tester
        // present, and CAN request/response ID defaults.
        "iso_14229_3" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_14229_3_on_iso_15765_2" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_15031_5_on_iso_14230_4" => Some(iso_15031_5_on_iso_14230_4()),
        "iso_15031_5_on_iso_15765_4" => Some(iso_15031_5_on_iso_15765_4()),
        "iso_15031_5_on_iso_9141_2" => Some(iso_15031_5_on_iso_9141_2()),
        // Combined K-line resource (ADR-069, resource 0x0212): same channel
        // behavior as the plain ISO9141 preset above.
        "iso_15031_5_on_iso_9141_2_and_iso_14230_4" => Some(iso_15031_5_on_iso_9141_2()),
        "iso_15031_5_on_sae_j1850_pwm" => Some(iso_15031_5_on_sae_j1850_pwm()),
        "iso_15031_5_on_sae_j1850_vpw" => Some(iso_15031_5_on_sae_j1850_vpw()),
        // Bus-agnostic VPW/PWM resource (ADR-069, resource 0x021C): parallels
        // the VPW-named preset above (the combined bus connects via J1850VPW
        // per the resource table).
        "iso_15031_5_on_sae_j1850" => Some(iso_15031_5_on_sae_j1850_vpw()),
        "iso_15765_3_on_iso_15765_2" => Some(iso_15765_3_on_iso_15765_2()),
        // Standalone ISO_15765_3 (ADR-069, resource 0x0207): spec-identical
        // to ISO_15765_3_on_ISO_15765_2 above.
        "iso_15765_3" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_obd_on_iso_15765_4" => Some(iso_obd_on_iso_15765_4()),
        "iso_obd_on_k_line" => Some(iso_obd_on_k_line()),
        "iso_obd_on_sae_j1850" => Some(iso_obd_on_sae_j1850()),
        "iso_obd_on_sae_j1939_73" => Some(iso_obd_on_sae_j1939_73()),
        "sae_j1587_on_sae_j1708" => Some(sae_j1587_on_sae_j1708()),
        "sae_j1939_73_on_sae_j1939_21" => Some(sae_j1939_73_on_sae_j1939_21()),
        "sae_j2190_on_iso_14230_2" => Some(sae_j2190_on_iso_14230_2()),
        "sae_j2190_on_iso_15765_2" => Some(sae_j2190_on_iso_15765_2()),
        "sae_j2190_on_iso_9141_2" => Some(sae_j2190_on_iso_9141_2()),
        // Combined K-line resource (ADR-069, resource 0x0214): parallels the
        // plain ISO9141 preset above.
        "sae_j2190_on_iso_9141_2_and_iso_14230_2" => Some(sae_j2190_on_iso_9141_2()),
        "sae_j2190_on_sae_j1850_pwm" => Some(sae_j2190_on_sae_j1850_pwm()),
        "sae_j2190_on_sae_j1850_vpw" => Some(sae_j2190_on_sae_j1850_vpw()),
        // Bus-agnostic VPW/PWM resource (ADR-069, resource 0x021A): parallels
        // the VPW-named preset above (the combined bus connects via J1850VPW
        // per the resource table).
        "sae_j2190_on_sae_j1850" => Some(sae_j2190_on_sae_j1850_vpw()),
        "sae_j2610_on_sae_j2610_sci" => Some(sae_j2610_on_sae_j2610_sci()),
        // Per-configuration SCI resources (ADR-069, resources 0x021F-0x0222):
        // all four configurations share the SAE_J2610_SCI channel's defaults.
        "sae_j2610_sci" => Some(sae_j2610_on_sae_j2610_sci()),
        // ── ADR-164/Phase 4: SAE J2534-2 clause 9 Single Wire CAN (SWCAN) ──
        // Each `_SWCAN`-suffixed resource row (resources.rs 0x0226-0x022F)
        // is a distinct-`protocol_name` sibling of one of the dual-wire
        // arms above -- disambiguation from its sibling in the resources
        // table (a Codex-style correction: a shared exact `protocol_name`
        // made `find_table_row_by_name` treat every pre-existing bare-name
        // lookup of these protocols as ambiguous, breaking three
        // pre-existing tests). The underlying application-layer protocol
        // identity, and therefore its own ComParam defaults (P2/P3 timing,
        // CAN addressing, etc.), is unaffected by which CAN bus type
        // carries it -- so each SWCAN row's defaults are the SAME preset
        // its dual-wire sibling resolves to above, layered on top of the
        // `SAE_J2411_SWCAN` bustype defaults (`bustype_default_params`)
        // instead of `ISO_11898_2_DWCAN`'s. `iso_15765_2_swcan` (0x022B,
        // the SWCAN sibling of native ISO15765 resource 0x0206) is
        // deliberately NOT listed here -- like its sibling, it has no
        // protocol-layer preset at all (see `PROTOCOL_NAMES_WITHOUT_
        // PROTOCOL_DEFAULTS` below), only the bustype defaults.
        "iso_11898_raw_swcan" => Some(iso_11898_raw()),
        "iso_14229_3_swcan" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_14229_3_on_iso_15765_2_swcan" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_14230_3_on_iso_15765_2_swcan" => Some(iso_14230_3_on_iso_15765_2()),
        "iso_15031_5_on_iso_15765_4_swcan" => Some(iso_15031_5_on_iso_15765_4()),
        "iso_15765_3_swcan" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_15765_3_on_iso_15765_2_swcan" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_obd_on_iso_15765_4_swcan" => Some(iso_obd_on_iso_15765_4()),
        "sae_j2190_on_iso_15765_2_swcan" => Some(sae_j2190_on_iso_15765_2()),
        // ── ADR-168/Phase 6: SAE J2534-2 clause 20 Fault-Tolerant CAN (FTCAN) ──
        // Same reasoning as the `_SWCAN` block above: each `_FTCAN`-suffixed
        // resource row (resources.rs 0x0230-0x0239) is a distinct-`protocol_name`
        // sibling of one of the dual-wire arms above, disambiguated in the
        // resources table the same way the SWCAN rows are. The underlying
        // application-layer protocol identity is unaffected by which CAN bus
        // type carries it, so each FTCAN row's defaults are the SAME preset
        // its dual-wire sibling resolves to above, layered on top of the
        // `ISO_11898_3_DWFTCAN` bustype defaults (`bustype_default_params`)
        // instead of `ISO_11898_2_DWCAN`'s. `iso_15765_2_ftcan` (0x0235, the
        // FTCAN sibling of native ISO15765 resource 0x0206) is deliberately
        // NOT listed here -- like its sibling and its SWCAN counterpart, it
        // has no protocol-layer preset at all (see
        // `PROTOCOL_NAMES_WITHOUT_PROTOCOL_DEFAULTS` above), only the
        // bustype defaults.
        "iso_11898_raw_ftcan" => Some(iso_11898_raw()),
        "iso_14229_3_ftcan" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_14229_3_on_iso_15765_2_ftcan" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_14230_3_on_iso_15765_2_ftcan" => Some(iso_14230_3_on_iso_15765_2()),
        "iso_15031_5_on_iso_15765_4_ftcan" => Some(iso_15031_5_on_iso_15765_4()),
        "iso_15765_3_ftcan" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_15765_3_on_iso_15765_2_ftcan" => Some(iso_15765_3_on_iso_15765_2()),
        "iso_obd_on_iso_15765_4_ftcan" => Some(iso_obd_on_iso_15765_4()),
        "sae_j2190_on_iso_15765_2_ftcan" => Some(sae_j2190_on_iso_15765_2()),
        _ => None,
    }?;
    // ADR-102 removes the ADR-056 CP_P2Star/CP_RC78CompletionTimeout preset
    // sync: CP_P2Star is now a reload-per-occurrence window (ISO 14229-2
    // §7.3 P2*client semantics), not a ceiling-scale value, so seeding it
    // from each preset's CP_RC78CompletionTimeout no longer makes sense.
    // Presets keep their existing CP_RC78CompletionTimeout values, which
    // ADR-102 reinstates as the (now-consulted again) total-duration
    // ceiling; CP_P2Star falls back to `RcHandlingConfig::from_params`'s own
    // 5000 ms default for every preset that doesn't explicitly set it.
    Some(p)
}

// ── STRUCTFIELD helpers ───────────────────────────────────────────────────────

pub(super) fn access_timing_zero() -> vci_service_interface::ParamStructfield {
    vci_service_interface::ParamStructfield {
        data: Some(
            vci_service_interface::param_structfield::Data::AccessTiming(
                vci_service_interface::ParamAccessTimingList {
                    entries: vec![vci_service_interface::ParamAccessTiming {
                        p2_min: 0,
                        p2_max: 0,
                        p3_min: 0,
                        p3_max: 0,
                        p4_min: 0,
                        timing_set: 0,
                    }],
                },
            ),
        ),
    }
}

pub(super) fn session_timing_empty() -> vci_service_interface::ParamStructfield {
    vci_service_interface::ParamStructfield {
        data: Some(
            vci_service_interface::param_structfield::Data::SessionTiming(
                vci_service_interface::ParamSessionTimingList { entries: vec![] },
            ),
        ),
    }
}

/// The "not enabled" empty shape for `CP_ExtendedTiming` when this protocol
/// never seeds it at all (ISO 22900-2 Table B.19: "ParamActLen = 0 (not
/// enabled)") -- deliberately NOT `access_timing_zero()`, which is a
/// *meaningful seeded default* (one all-zero-valued entry) for the specific
/// presets that call it, not the "nothing configured" shape `GetComParam`
/// must report when a protocol allows this param but no preset seeds it
/// (A2-18; see ADR-133).
pub(super) fn access_timing_empty() -> vci_service_interface::ParamStructfield {
    vci_service_interface::ParamStructfield {
        data: Some(
            vci_service_interface::param_structfield::Data::AccessTiming(
                vci_service_interface::ParamAccessTimingList { entries: vec![] },
            ),
        ),
    }
}

// ── Protocol functions ────────────────────────────────────────────────────────

fn iso_11898_raw() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 50_000);
    p.unum32.insert(ComParamId(j2534_0404::P2_MIN), 0);
    p.unum32.insert(PARAM_P3_FUNC, 0);
    p.unum32.insert(PARAM_P3_PHYS, 0);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RATE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RES_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_TX_DELAY, 0);
    p.unum32.insert(PARAM_SW_CAN_HIGH_VOLTAGE, 0);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE, 0);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE_HANDLING, 1);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_FORMAT, 0);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_FORMAT, 0);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_ID, 0x5E0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_FORMAT, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_ID, 0xFFFF_FFFF);
    p.unum32.insert(PARAM_CANFD_TX_MAX_DATA_LENGTH, 0);
    p.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x01);
    p.bytes.insert(PARAM_CHANGE_SPEED_MSG, vec![]);
    p
}

fn kwp_on_kline_common(p: &mut ComParamSet, req_addr_mode: u32, header_format: u32) {
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MIN), 25_000);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 50_000);
    p.unum32.insert(ComParamId(j2534_0404::P3_MIN), 55_000);
    p.unum32.insert(ComParamId(j2534_0404::P1_MIN), 0);
    p.unum32.insert(ComParamId(j2534_0404::P1_MAX), 20_000);
    p.unum32.insert(ComParamId(j2534_0404::P4_MIN), 5_000);
    p.unum32.insert(ComParamId(j2534_0404::P4_MAX), 20_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC21_HANDLING, 0);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 0);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_START_MSG_IND_ENABLE, 0);
    p.unum32.insert(PARAM_SUSPEND_QUEUE_ON_ERROR, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_HANDLING, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_REQ_RSP, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_SEND_TYPE, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_INTERVAL_US, 3_000_000);
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 1);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RATE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RES_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_TX_DELAY, 0);
    p.unum32.insert(PARAM_ECU_RESP_SOURCE_ADDR, 0x10);
    p.unum32.insert(PARAM_ENABLE_CONCATENATION, 0);
    p.unum32.insert(PARAM_HEADER_FORMAT_KW, header_format);
    p.unum32.insert(PARAM_FUNC_REQ_FORMAT_PRIORITY, 0xC0);
    p.unum32.insert(PARAM_FUNC_REQ_TARGET_ADDR, 0x33);
    p.unum32.insert(PARAM_FUNC_RESP_FORMAT_PRIORITY, 0xC0);
    p.unum32.insert(PARAM_FUNC_RESP_TARGET_ADDR, 0xF1);
    p.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, 0x80);
    p.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
    p.unum32.insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, 0x80);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_TRANS, 0);
    p.unum32.insert(PARAM_REQUEST_ADDR_MODE, req_addr_mode);
    p.unum32.insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
    p.unum32.insert(ComParamId(j2534_0404::TIDLE), 300_000);
    p.unum32.insert(ComParamId(j2534_0404::TINIL), 25_000);
    p.unum32.insert(ComParamId(j2534_0404::TWUP), 50_000);
    p.unum32.insert(ComParamId(j2534_0404::W1), 300_000); // CP_W1Max
    p.unum32.insert(ComParamId(j2534_0404::W2), 20_000); // CP_W2Max
    p.unum32.insert(ComParamId(j2534_0404::W3), 20_000); // CP_W3Max
    // ADR-181: the native W4 slot is CP_W4Min's own register (Figure 30
    // defines no native MAX-side W4), so it seeds CP_W4Min's ISO default,
    // not CP_W4Max's -- previously mislabeled/miscoded as CP_W4Max's
    // 50_000 us default, which silently forwarded the wrong value to
    // hardware.
    p.unum32.insert(ComParamId(j2534_0404::W4), 25_000); // CP_W4Min
    p.unum32.insert(PARAM_W1_MIN, 60_000);
    p.unum32.insert(PARAM_W2_MIN, 5_000);
    p.unum32.insert(PARAM_W3_MIN, 0);
    p.unum32.insert(PARAM_W4_MAX, 50_000);
    p.unum32.insert(ComParamId(j2534_0404::FIVE_BAUD_MOD), 0);
    p.unum32.insert(PARAM_5BAUD_ADDR_FUNC, 0x33);
    p.unum32.insert(PARAM_5BAUD_ADDR_PHYS, 0x01);
    p.unum32.insert(PARAM_5BAUD_COMM_BAUDRATE_OVERRIDE, 0);
    p.unum32.insert(PARAM_5BAUD_INIT_BAUDRATE, 200_000);
    p.unum32.insert(PARAM_ISO_KEYBYTE_COUNT, 2);
    p.unum32.insert(PARAM_NUM_HEADER_BYTES_START_COMM_KW, 3);
    p.unum32.insert(PARAM_FILLER_BYTE, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_HANDLING, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_LENGTH, 0);
    p.bytes.insert(PARAM_CHANGE_SPEED_MSG, vec![]);
}

fn iso_14230_3_on_iso_14230_2() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK, 0);
    p.unum32.insert(PARAM_TEST_MODE, 0);
    p.unum32.insert(PARAM_ENABLE_INIT_SEQ_REPETITION, 0);
    p.unum32.insert(PARAM_MODIFY_TIMING, 0);
    p.unum32.insert(ComParamId(j2534_0404::P3_MAX), 5_000_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 1_300_000);
    p.unum32.insert(PARAM_RC21_HANDLING, 0);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 0);
    p.unum32.insert(PARAM_INIT_SETTINGS, 0x02);
    p.unum32.insert(PARAM_IGNORE_CHECKSUM, 1);
    kwp_on_kline_common(&mut p, 0x01, 0);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0); // override common
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x7E]);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![0x7F, 0x3E]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x3E]);
    p.structfield
        .insert(PARAM_EXTENDED_TIMING, access_timing_zero());
    p
}

fn iso_15031_5_on_iso_14230_4() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK, 0);
    p.unum32.insert(PARAM_TEST_MODE, 0);
    p.unum32.insert(PARAM_ENABLE_INIT_SEQ_REPETITION, 0);
    p.unum32.insert(PARAM_MODIFY_TIMING, 0);
    p.unum32.insert(ComParamId(j2534_0404::P3_MAX), 5_000_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 1_300_000);
    p.unum32.insert(PARAM_RC21_HANDLING, 2);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 30_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 2);
    p.unum32.insert(PARAM_INIT_SETTINGS, 0x02);
    p.unum32.insert(PARAM_IGNORE_CHECKSUM, 1);
    kwp_on_kline_common(&mut p, 0x02, 0);
    // PARAM_TESTER_PRESENT_ADDR_MODE: no override here -- ISO_14230_4's
    // Table B.20 default is 0 (physical), same as kwp_on_kline_common's own
    // seed (ADR-138), so no explicit insert is needed.
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x41, 0x00]);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![0x7F]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x01, 0x00]);
    p.structfield
        .insert(PARAM_EXTENDED_TIMING, access_timing_zero());
    p
}

fn sae_j2190_on_iso_14230_2() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK, 0);
    p.unum32.insert(PARAM_TEST_MODE, 0);
    p.unum32.insert(PARAM_MODIFY_TIMING, 0);
    p.unum32.insert(ComParamId(j2534_0404::P3_MAX), 5_000_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC21_HANDLING, 0);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 0);
    p.unum32.insert(PARAM_INIT_SETTINGS, 0x02);
    p.unum32.insert(PARAM_IGNORE_CHECKSUM, 1);
    kwp_on_kline_common(&mut p, 0x01, 0);
    p.unum32.insert(PARAM_ENABLE_INIT_SEQ_REPETITION, 0);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x7E]);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![0x7F, 0x3E]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x3E]);
    p.structfield
        .insert(PARAM_EXTENDED_TIMING, access_timing_zero());
    p
}

/// Addressing-format parameters shared by the KWP-on-ISO9141 protocol
/// variants, grouped to keep [`kwp_on_9141_common`] under clippy's
/// `too_many_arguments` threshold.
struct Kwp9141Params {
    req_addr_mode: u32,
    func_format: u32,
    func_target: u32,
    func_resp_format: u32,
    func_resp_target: u32,
    phys_format: u32,
    phys_resp_format: u32,
}

fn kwp_on_9141_common(p: &mut ComParamSet, params: Kwp9141Params) {
    let Kwp9141Params {
        req_addr_mode,
        func_format,
        func_target,
        func_resp_format,
        func_resp_target,
        phys_format,
        phys_resp_format,
    } = params;
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MIN), 25_000);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 50_000);
    p.unum32.insert(ComParamId(j2534_0404::P3_MIN), 55_000);
    p.unum32.insert(ComParamId(j2534_0404::P1_MIN), 0);
    p.unum32.insert(ComParamId(j2534_0404::P1_MAX), 20_000);
    p.unum32.insert(ComParamId(j2534_0404::P4_MIN), 5_000);
    p.unum32.insert(ComParamId(j2534_0404::P4_MAX), 20_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC21_HANDLING, 0);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 0);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_START_MSG_IND_ENABLE, 0);
    p.unum32.insert(PARAM_SUSPEND_QUEUE_ON_ERROR, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_HANDLING, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_REQ_RSP, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_SEND_TYPE, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_INTERVAL_US, 2_000_000);
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 1);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RATE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RES_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_TX_DELAY, 0);
    p.unum32.insert(PARAM_ECU_RESP_SOURCE_ADDR, 0x10);
    p.unum32.insert(PARAM_ENABLE_CONCATENATION, 0);
    p.unum32.insert(PARAM_HEADER_FORMAT_KW, 0);
    p.unum32.insert(PARAM_FUNC_REQ_FORMAT_PRIORITY, func_format);
    p.unum32.insert(PARAM_FUNC_REQ_TARGET_ADDR, func_target);
    p.unum32
        .insert(PARAM_FUNC_RESP_FORMAT_PRIORITY, func_resp_format);
    p.unum32
        .insert(PARAM_FUNC_RESP_TARGET_ADDR, func_resp_target);
    p.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, phys_format);
    p.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
    p.unum32
        .insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, phys_resp_format);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_TRANS, 0);
    p.unum32.insert(PARAM_REQUEST_ADDR_MODE, req_addr_mode);
    p.unum32.insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
    p.unum32.insert(ComParamId(j2534_0404::TIDLE), 300_000);
    p.unum32.insert(ComParamId(j2534_0404::TINIL), 25_000);
    p.unum32.insert(ComParamId(j2534_0404::TWUP), 50_000);
    p.unum32.insert(ComParamId(j2534_0404::W1), 300_000);
    p.unum32.insert(ComParamId(j2534_0404::W2), 20_000);
    p.unum32.insert(ComParamId(j2534_0404::W3), 20_000);
    // ADR-181: native W4 is CP_W4Min's own register -- see
    // `kwp_on_kline_common`'s identical fix for the full rationale.
    p.unum32.insert(ComParamId(j2534_0404::W4), 25_000);
    p.unum32.insert(PARAM_W1_MIN, 60_000);
    p.unum32.insert(PARAM_W2_MIN, 5_000);
    p.unum32.insert(PARAM_W3_MIN, 0);
    p.unum32.insert(PARAM_W4_MAX, 50_000);
    p.unum32.insert(ComParamId(j2534_0404::FIVE_BAUD_MOD), 0);
    p.unum32.insert(PARAM_5BAUD_ADDR_FUNC, 0x33);
    p.unum32.insert(PARAM_5BAUD_ADDR_PHYS, 0x01);
    p.unum32.insert(PARAM_5BAUD_COMM_BAUDRATE_OVERRIDE, 0);
    p.unum32.insert(PARAM_5BAUD_INIT_BAUDRATE, 200_000);
    p.unum32.insert(PARAM_ISO_KEYBYTE_COUNT, 2);
    p.unum32.insert(PARAM_FILLER_BYTE, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_HANDLING, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_LENGTH, 0);
    p.bytes.insert(PARAM_CHANGE_SPEED_MSG, vec![]);
}

fn iso_15031_5_on_iso_9141_2() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK, 0);
    p.unum32.insert(PARAM_TEST_MODE, 0);
    p.unum32.insert(PARAM_ENABLE_INIT_SEQ_REPETITION, 0);
    kwp_on_9141_common(
        &mut p,
        Kwp9141Params {
            req_addr_mode: 0x02,
            func_format: 0x68,
            func_target: 0x6A,
            func_resp_format: 0x48,
            func_resp_target: 0x6B,
            phys_format: 0x6C,
            phys_resp_format: 0x6C,
        },
    );
    p.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0);
    p.unum32.insert(PARAM_INIT_SETTINGS, 0x01);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x41, 0x00]);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![0x7F]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x01, 0x00]);
    p
}

fn sae_j2190_on_iso_9141_2() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_DISABLE_TRANSPORT_CHECKSUM_CHECK, 0);
    p.unum32.insert(PARAM_TEST_MODE, 0);
    p.unum32.insert(PARAM_ENABLE_INIT_SEQ_REPETITION, 0);
    p.unum32.insert(PARAM_IGNORE_CHECKSUM, 1);
    kwp_on_9141_common(
        &mut p,
        Kwp9141Params {
            req_addr_mode: 0x02,
            func_format: 0x68,
            func_target: 0x6A,
            func_resp_format: 0x48,
            func_resp_target: 0x6B,
            phys_format: 0x6C,
            phys_resp_format: 0x6C,
        },
    );
    p.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0);
    p.unum32.insert(PARAM_INIT_SETTINGS, 0x01);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.unum32.insert(PARAM_5BAUD_ADDR_FUNC, 0x33); // override
    p.unum32.insert(PARAM_5BAUD_ADDR_PHYS, 0x01);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x41, 0x00]);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![0x7F]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x01, 0x00]);
    p
}

fn iso_obd_on_k_line() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_ENABLE_INIT_SEQ_REPETITION, 0);
    p.unum32.insert(PARAM_MODIFY_TIMING, 0); // from table (not in common)
    p.unum32.insert(ComParamId(j2534_0404::P3_MAX), 5_000_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 1_300_000);
    p.unum32.insert(PARAM_RC21_HANDLING, 0);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 2);
    // CP_InitializationSettings = 0x02 (fast-init): valid on this resource's
    // ISO9141 connect protocol (ADR-069) -- J2534-1 v04.04 FAST_INIT applies
    // to both K-line protocols (ISO9141 and ISO14230), not ISO14230 alone.
    p.unum32.insert(PARAM_INIT_SETTINGS, 0x02);
    p.unum32.insert(PARAM_IGNORE_CHECKSUM, 1);
    kwp_on_kline_common(&mut p, 0x02, 0);
    // ISO_OBD_on_K_Line maps to the underlying protocol ISO_9141_2 (Annex D.1
    // Table D.4 line 5251: the combined OBD-on-K-Line entry for 9141-2 and KWP2000),
    // which Table B.19 does not define a CP_RC21RequestTime/CP_RC23RequestTime/
    // CP_TesterPresentTime row for at all -- unlike ISO_14230_3/ISO_14230_4/
    // SAE_J2190, which do have rows and are the only presets A2-16 verified
    // and fixed. `kwp_on_kline_common` is shared with those three, so its
    // K-line-wide correction would otherwise silently leak into this
    // unverified, spec-undefined resource too. Restore this preset's original
    // values here (Codex review, PR #120): a Table B.19 gap is not evidence
    // the old value was wrong, and re-verifying it needs the same file:line
    // rigor the rest of A2-16 applied, not a guess in either direction.
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_TESTER_PRESENT_INTERVAL_US, 2_000_000);
    p.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x41, 0x00]);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![0x7F]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x01, 0x00]);
    p.structfield
        .insert(PARAM_EXTENDED_TIMING, access_timing_zero());
    p
}

fn can_iso15765_common(p: &mut ComParamSet) {
    p.unum32.insert(PARAM_CHANGE_SPEED_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RATE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RES_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_TX_DELAY, 0);
    p.unum32.insert(PARAM_SW_CAN_HIGH_VOLTAGE, 0);
    p.unum32.insert(PARAM_CAN_MIXED_FORMAT, 0);
    p.unum32.insert(PARAM_CANFD_TX_MAX_DATA_LENGTH, 0);
    p.unum32.insert(PARAM_ESCAPE_SEQUENCE_HANDLING, 0);
    p.unum32.insert(PARAM_MAX_DATA_LENGTH_ECU, 4095);
    p.bytes.insert(PARAM_CHANGE_SPEED_MSG, vec![]);
}

fn iso_14230_3_on_iso_15765_2() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_MODIFY_TIMING, 0);
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MIN), 25_000);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 50_000);
    p.unum32.insert(ComParamId(j2534_0404::P3_MIN), 55_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 1_300_000);
    p.unum32.insert(PARAM_RC21_HANDLING, 0);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 2);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_START_MSG_IND_ENABLE, 0);
    p.unum32.insert(PARAM_SUSPEND_QUEUE_ON_ERROR, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_HANDLING, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_REQ_RSP, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_SEND_TYPE, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_INTERVAL_US, 2_000_000);
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 1);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_TRANS, 0);
    p.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x01);
    // ISO15765 TP timers
    p.unum32.insert(PARAM_N_AR, 1_000_000);
    p.unum32.insert(PARAM_N_AS, 1_000_000);
    p.unum32.insert(PARAM_N_BR, 0);
    p.unum32.insert(PARAM_N_BS, 1_000_000);
    p.unum32.insert(PARAM_N_CR, 1_000_000);
    p.unum32.insert(PARAM_N_CS, 0);
    // ISO15765 hardware
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_BS), 0);
    p.unum32.insert(ComParamId(j2534_0404::BS_TX), 0xFFFF);
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_STMIN), 0);
    p.unum32
        .insert(ComParamId(j2534_0404::STMIN_TX), 0xFFFF_FFFF);
    p.unum32
        .insert(ComParamId(j2534_0404::ISO15765_WFT_MAX), 255);
    // CAN addressing
    p.unum32.insert(PARAM_CAN_DATA_SIZE_OFFSET, 0);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE, 0x55);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE_HANDLING, 1);
    p.unum32.insert(PARAM_CAN_FIRST_CF_VALUE, 0x01);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_EXT_ADDR, 0x00);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_ID, 0x7E0);
    p.unum32.insert(PARAM_CAN_RESP_USDT_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_RESP_USDT_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_RESP_USDT_ID, 0x7E8);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_FORMAT, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_ID, 0xFFFF_FFFF);
    can_iso15765_common(&mut p);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x7E]);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![0x7F, 0x3E]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x3E]);
    p
}

fn iso15765_4_common(p: &mut ComParamSet) {
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MIN), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 50_000);
    p.unum32.insert(PARAM_P3_FUNC, 50_000);
    p.unum32.insert(PARAM_P3_PHYS, 50_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 1_300_000);
    p.unum32.insert(PARAM_RC21_HANDLING, 2);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 30_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 2);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_START_MSG_IND_ENABLE, 0);
    p.unum32.insert(PARAM_SUSPEND_QUEUE_ON_ERROR, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_HANDLING, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_REQ_RSP, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_SEND_TYPE, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_INTERVAL_US, 3_000_000);
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 1);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_TRANS, 0);
    p.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x02);
    // ISO15765 TP timers
    p.unum32.insert(PARAM_N_AR, 1_000_000);
    p.unum32.insert(PARAM_N_AS, 25_000);
    p.unum32.insert(PARAM_N_BR, 0);
    p.unum32.insert(PARAM_N_BS, 75_000);
    p.unum32.insert(PARAM_N_CR, 150_000);
    p.unum32.insert(PARAM_N_CS, 0);
    // ISO15765 hardware
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_BS), 0);
    p.unum32.insert(ComParamId(j2534_0404::BS_TX), 0xFFFF);
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_STMIN), 0);
    p.unum32
        .insert(ComParamId(j2534_0404::STMIN_TX), 0xFFFF_FFFF);
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_WFT_MAX), 0);
    // CAN addressing
    p.unum32.insert(PARAM_CAN_DATA_SIZE_OFFSET, 0);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE, 0x00);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE_HANDLING, 1);
    p.unum32.insert(PARAM_CAN_FIRST_CF_VALUE, 0x01);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_EXT_ADDR, 0x00);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_ID, 0x7E0);
    p.unum32.insert(PARAM_CAN_RESP_USDT_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_RESP_USDT_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_RESP_USDT_ID, 0x7E8);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_FORMAT, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_ID, 0xFFFF_FFFF);
    can_iso15765_common(p);
}

fn iso_15031_5_on_iso_15765_4() -> ComParamSet {
    let mut p = ComParamSet::default();
    iso15765_4_common(&mut p);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x41, 0x00]);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![0x7F]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x01, 0x00]);
    p
}

fn iso_obd_on_iso_15765_4() -> ComParamSet {
    let mut p = ComParamSet::default();
    iso15765_4_common(&mut p);
    // OBD uses functional ext addr 0xFE
    p.unum32.insert(PARAM_CAN_FUNC_REQ_EXT_ADDR, 0xFE);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x41, 0x00]);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![0x7F]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x01, 0x00]);
    p
}

fn iso_15765_3_on_iso_15765_2() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_CAN_TRANSMISSION_TIME, 100_000);
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(PARAM_MODIFY_TIMING, 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MIN), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 150_000);
    p.unum32.insert(PARAM_P3_FUNC, 50_000);
    p.unum32.insert(PARAM_P3_PHYS, 50_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 1_300_000);
    p.unum32.insert(PARAM_RC21_HANDLING, 0);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 10_000);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 2);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_START_MSG_IND_ENABLE, 0);
    p.unum32.insert(PARAM_SUSPEND_QUEUE_ON_ERROR, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_HANDLING, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_REQ_RSP, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_SEND_TYPE, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_INTERVAL_US, 2_000_000);
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 1);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_TRANS, 0);
    p.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x01);
    // ISO15765 TP timers
    p.unum32.insert(PARAM_N_AR, 1_000_000);
    p.unum32.insert(PARAM_N_AS, 1_000_000);
    p.unum32.insert(PARAM_N_BR, 0);
    p.unum32.insert(PARAM_N_BS, 1_000_000);
    p.unum32.insert(PARAM_N_CR, 1_000_000);
    p.unum32.insert(PARAM_N_CS, 0);
    // ISO15765 hardware
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_BS), 0);
    p.unum32.insert(ComParamId(j2534_0404::BS_TX), 0xFFFF);
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_STMIN), 0);
    p.unum32
        .insert(ComParamId(j2534_0404::STMIN_TX), 0xFFFF_FFFF);
    p.unum32
        .insert(ComParamId(j2534_0404::ISO15765_WFT_MAX), 255);
    // CAN addressing
    p.unum32.insert(PARAM_CAN_DATA_SIZE_OFFSET, 0);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE, 0x55);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE_HANDLING, 1);
    p.unum32.insert(PARAM_CAN_FIRST_CF_VALUE, 0x01);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_EXT_ADDR, 0x00);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_ID, 0x7E0);
    p.unum32.insert(PARAM_CAN_RESP_USDT_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_RESP_USDT_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_RESP_USDT_ID, 0x7E8);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_FORMAT, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_ID, 0xFFFF_FFFF);
    can_iso15765_common(&mut p);
    p.bytes
        .insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![0x7E]);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x3E, 0x80]);
    p.structfield
        .insert(PARAM_SESSION_TIMING_OVERRIDE, session_timing_empty());
    p
}

fn sae_j2190_on_iso_15765_2() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MIN), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 50_000);
    p.unum32.insert(PARAM_P3_FUNC, 0);
    p.unum32.insert(PARAM_P3_PHYS, 0);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC21_HANDLING, 0);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 200_000);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 0);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_START_MSG_IND_ENABLE, 0);
    p.unum32.insert(PARAM_SUSPEND_QUEUE_ON_ERROR, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_HANDLING, 1);
    p.unum32.insert(PARAM_TESTER_PRESENT_REQ_RSP, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_SEND_TYPE, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_INTERVAL_US, 2_000_000);
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 1);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_TRANS, 0);
    p.unum32.insert(PARAM_REQUEST_ADDR_MODE, 0x01);
    // ISO15765 TP timers
    p.unum32.insert(PARAM_N_AR, 1_000_000);
    p.unum32.insert(PARAM_N_AS, 1_000_000);
    p.unum32.insert(PARAM_N_BR, 0);
    p.unum32.insert(PARAM_N_BS, 1_000_000);
    p.unum32.insert(PARAM_N_CR, 1_000_000);
    p.unum32.insert(PARAM_N_CS, 0);
    // ISO15765 hardware
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_BS), 0);
    p.unum32.insert(ComParamId(j2534_0404::BS_TX), 0xFFFF);
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_STMIN), 0);
    p.unum32
        .insert(ComParamId(j2534_0404::STMIN_TX), 0xFFFF_FFFF);
    p.unum32
        .insert(ComParamId(j2534_0404::ISO15765_WFT_MAX), 255);
    // CAN addressing
    p.unum32.insert(PARAM_CAN_DATA_SIZE_OFFSET, 0);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE, 0x55);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE_HANDLING, 1);
    p.unum32.insert(PARAM_CAN_FIRST_CF_VALUE, 0x01);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_EXT_ADDR, 0x00);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_FUNC_REQ_ID, 0x7DF);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_PHYS_REQ_ID, 0x7E0);
    p.unum32.insert(PARAM_CAN_RESP_USDT_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_RESP_USDT_FORMAT, 0x05);
    p.unum32.insert(PARAM_CAN_RESP_USDT_ID, 0x7E8);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_EXT_ADDR, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_FORMAT, 0);
    p.unum32.insert(PARAM_CAN_RESP_UUDT_ID, 0xFFFF_FFFF);
    can_iso15765_common(&mut p);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x3F]);
    p
}

fn j1939_can_common(p: &mut ComParamSet) {
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(PARAM_CAN_MIXED_FORMAT, 2_147_483_649);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 200_000);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_START_MSG_IND_ENABLE, 0);
    p.unum32.insert(PARAM_SUSPEND_QUEUE_ON_ERROR, 0);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_MESSAGE_INDICATION_RATE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RATE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RES_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_TX_DELAY, 0);
    // ISO15765 TP timers (J1939 TP uses same hardware)
    p.unum32.insert(PARAM_N_BR, 500_000);
    p.unum32.insert(PARAM_N_BS, 1_050_000);
    p.unum32.insert(PARAM_N_CR, 750_000);
    p.unum32.insert(PARAM_N_CS, 50_000);
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_BS), 0);
    p.unum32.insert(ComParamId(j2534_0404::BS_TX), 0xFFFF);
    p.unum32
        .insert(ComParamId(j2534_0404::ISO15765_WFT_MAX), 255);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE, 0x00);
    p.unum32.insert(PARAM_CAN_FILLER_BYTE_HANDLING, 0);
    // J1939 addressing
    p.unum32.insert(PARAM_J1939_ADDR_CLAIM_TIMEOUT, 1_250_000);
    p.unum32.insert(PARAM_J1939_DATA_PAGE, 0x00);
    p.unum32.insert(PARAM_J1939_MAX_PACKET_TX, 0xFF);
    p.unum32.insert(PARAM_J1939_TARGET_ADDRESS, 0xFFFF);
    p.unum32.insert(PARAM_J1939_SOURCE_ADDRESS, 0);
    p.unum32.insert(PARAM_J1939_ADDR_NEG_RULE, 0);
    p.unum32.insert(PARAM_J1939_PDU_FORMAT, 0x00);
    p.unum32.insert(PARAM_J1939_PDU_SPECIFIC, 0x00);
    p.unum32.insert(PARAM_MESSAGE_PRIORITY, 6);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_TRANS, 0);
    p.unum32.insert(PARAM_SEND_REMOTE_FRAME, 0);
    // J1939 TP timers (ADR-179 Decision 5/Context: the same ISO 22900-2
    // CP_T3Max/CP_T4Max/CP_T5Max ComParams the SCI presets below also seed,
    // with J1939-specific per-protocol default values -- not a distinct
    // "service-level, not SCI T3/T4/T5" constant as an earlier version of
    // this comment claimed; see service_params.rs's own doc comment on this
    // retirement)
    p.unum32.insert(ComParamId(j2534_0404::T3_MAX), 200_000);
    p.unum32.insert(ComParamId(j2534_0404::T4_MAX), 1_250_000);
    p.unum32.insert(ComParamId(j2534_0404::T5_MAX), 1_250_000);
    p.bytes.insert(PARAM_J1939_NAME, vec![]);
    p.bytes.insert(PARAM_J1939_PREFERRED_ADDRESS, vec![]);
    p.bytes.insert(PARAM_J1939_TARGET_NAME, vec![]);
    p.bytes.insert(PARAM_J1939_SOURCE_NAME, vec![]);
    p.bytes.insert(PARAM_CHANGE_SPEED_MSG, vec![]);
}

fn iso_obd_on_sae_j1939_73() -> ComParamSet {
    let mut p = ComParamSet::default();
    j1939_can_common(&mut p);
    // OBD on J1939 has MessagePriority = 0
    p.unum32.insert(PARAM_MESSAGE_PRIORITY, 0);
    p
}

fn sae_j1939_73_on_sae_j1939_21() -> ComParamSet {
    let mut p = ComParamSet::default();
    j1939_can_common(&mut p);
    // J1939 uses BlockSize 0xFF
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_BS), 0xFF);
    p
}

/// Addressing-format parameters shared by the SAE J1850 protocol variants,
/// grouped to keep [`j1850_common`] under clippy's `too_many_arguments`
/// threshold.
struct J1850Params {
    func_format: u32,
    func_target: u32,
    func_resp_format: u32,
    func_resp_target: u32,
    phys_format: u32,
    phys_resp_format: u32,
    header_format: u32,
    req_addr_mode: u32,
}

fn j1850_common(p: &mut ComParamSet, params: J1850Params) {
    let J1850Params {
        func_format,
        func_target,
        func_resp_format,
        func_resp_target,
        phys_format,
        phys_resp_format,
        header_format,
        req_addr_mode,
    } = params;
    p.unum32.insert(PARAM_CHANGE_SPEED_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RATE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RES_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_TX_DELAY, 0);
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MIN), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 100_000);
    p.unum32.insert(PARAM_RC21_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC21_HANDLING, 0);
    p.unum32.insert(PARAM_RC21_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC23_COMPLETION_TIMEOUT, 0);
    p.unum32.insert(PARAM_RC23_HANDLING, 0);
    p.unum32.insert(PARAM_RC23_REQUEST_TIME, 0);
    p.unum32.insert(PARAM_RC78_COMPLETION_TIMEOUT, 25_000_000);
    p.unum32.insert(PARAM_RC78_HANDLING, 0);
    p.unum32.insert(PARAM_RC_BYTE_OFFSET, 0xFFFF_FFFF);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_SUSPEND_QUEUE_ON_ERROR, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_ADDR_MODE, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_HANDLING, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_REQ_RSP, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_SEND_TYPE, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_INTERVAL_US, 3_000_000);
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 0);
    p.unum32.insert(PARAM_ECU_RESP_SOURCE_ADDR, 0x10);
    p.unum32.insert(PARAM_FUNC_REQ_FORMAT_PRIORITY, func_format);
    p.unum32.insert(PARAM_FUNC_REQ_TARGET_ADDR, func_target);
    p.unum32
        .insert(PARAM_FUNC_RESP_FORMAT_PRIORITY, func_resp_format);
    p.unum32
        .insert(PARAM_FUNC_RESP_TARGET_ADDR, func_resp_target);
    p.unum32.insert(PARAM_HEADER_FORMAT_J1850, header_format);
    p.unum32.insert(PARAM_PHYS_REQ_FORMAT_PRIORITY, phys_format);
    p.unum32.insert(PARAM_PHYS_REQ_TARGET_ADDR, 0x10);
    p.unum32
        .insert(PARAM_PHYS_RESP_FORMAT_PRIORITY, phys_resp_format);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_TRANS, 0);
    p.unum32.insert(PARAM_REQUEST_ADDR_MODE, req_addr_mode);
    p.unum32.insert(ComParamId(j2534_0404::NODE_ADDRESS), 0xF1);
    p.bytes.insert(PARAM_CHANGE_SPEED_MSG, vec![]);
}

fn iso_15031_5_on_sae_j1850_pwm() -> ComParamSet {
    let mut p = ComParamSet::default();
    j1850_common(
        &mut p,
        J1850Params {
            // PWM header/priority bytes (spec correction: this function
            // previously carried the VPW byte values verbatim, a copy-paste
            // bug from `iso_15031_5_on_sae_j1850_vpw` -- func_format must be
            // `0x61` under PWM, matching `rpc_link::J1850_OBD_PROBE_PWM`'s
            // probe constant and `sae_j2190_on_sae_j1850_pwm`'s own PWM
            // values below; `phys_format`/`phys_resp_format` likewise follow
            // that same VPW/PWM split (`0xC4` for both under PWM, mirroring
            // `SAE_J2190_on_SAE_J1850_PWM`'s physical-addressing bytes).
            func_format: 0x61,
            func_target: 0x6A,
            func_resp_format: 0x41,
            func_resp_target: 0x6B,
            phys_format: 0xC4,
            phys_resp_format: 0xC4,
            header_format: 0x03,
            req_addr_mode: 0x02,
        },
    );
    p.unum32.insert(PARAM_TERMINATION_TYPE, 0);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![]);
    p
}

fn iso_15031_5_on_sae_j1850_vpw() -> ComParamSet {
    let mut p = ComParamSet::default();
    j1850_common(
        &mut p,
        J1850Params {
            func_format: 0x68,
            func_target: 0x6A,
            func_resp_format: 0x48,
            func_resp_target: 0x6B,
            phys_format: 0x6C,
            phys_resp_format: 0x2C,
            header_format: 0x03,
            req_addr_mode: 0x02,
        },
    );
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![]);
    p
}

fn iso_obd_on_sae_j1850() -> ComParamSet {
    let mut p = ComParamSet::default();
    j1850_common(
        &mut p,
        J1850Params {
            func_format: 0x68,
            func_target: 0x6A,
            func_resp_format: 0x48,
            func_resp_target: 0x6B,
            phys_format: 0x6C,
            phys_resp_format: 0x2C,
            header_format: 0x03,
            req_addr_mode: 0x02,
        },
    );
    p.unum32.insert(PARAM_ENABLE_CONCATENATION, 0);
    p.unum32.insert(PARAM_FILLER_BYTE, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_HANDLING, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_LENGTH, 0);
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 1); // override common
    p.unum32.insert(PARAM_TERMINATION_TYPE, 0);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![]);
    p
}

fn sae_j2190_on_sae_j1850_pwm() -> ComParamSet {
    let mut p = ComParamSet::default();
    j1850_common(
        &mut p,
        J1850Params {
            func_format: 0x61,
            func_target: 0x6A,
            func_resp_format: 0x41,
            func_resp_target: 0x6B,
            phys_format: 0xC4,
            phys_resp_format: 0xC4,
            header_format: 0x03,
            req_addr_mode: 0x02,
        },
    );
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 1); // override common
    p.unum32.insert(PARAM_ENABLE_CONCATENATION, 0);
    p.unum32.insert(PARAM_FILLER_BYTE, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_HANDLING, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_LENGTH, 0);
    p.unum32.insert(PARAM_TERMINATION_TYPE, 0);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x3F]);
    p
}

fn sae_j2190_on_sae_j1850_vpw() -> ComParamSet {
    let mut p = ComParamSet::default();
    j1850_common(
        &mut p,
        J1850Params {
            func_format: 0x68,
            func_target: 0x6A,
            func_resp_format: 0x48,
            func_resp_target: 0x6B,
            phys_format: 0x6C,
            phys_resp_format: 0x2C,
            header_format: 0x03,
            req_addr_mode: 0x02,
        },
    );
    p.unum32.insert(PARAM_TESTER_PRESENT_IMMED, 1); // override common
    p.unum32.insert(PARAM_ENABLE_CONCATENATION, 0);
    p.unum32.insert(PARAM_FILLER_BYTE, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_HANDLING, 0);
    p.unum32.insert(PARAM_FILLER_BYTE_LENGTH, 0);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_POS_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_EXP_NEG_RESP, vec![]);
    p.bytes.insert(PARAM_TESTER_PRESENT_MSG, vec![0x3F]);
    p
}

fn sae_j1587_on_sae_j1708() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_TP_CONNECTION_MGMT, 0);
    p.unum32.insert(ComParamId(j2534_0404::ISO15765_BS), 0xFF);
    p.unum32.insert(ComParamId(j2534_0404::BS_TX), 0xFFFF);
    p.unum32.insert(PARAM_N_BS, 60_000_000);
    p.unum32.insert(PARAM_N_CR, 1_000_000);
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(ComParamId(j2534_0404::P2_MAX), 60_000_000);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_START_MSG_IND_ENABLE, 0);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RATE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RES_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_TX_DELAY, 0);
    p.unum32.insert(PARAM_MID_REQ_ID, 0);
    p.unum32.insert(PARAM_MID_RESP_ID, 0);
    p.unum32.insert(PARAM_MESSAGE_PRIORITY, 8);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_TRANS, 0);
    p.unum32.insert(PARAM_MAX_CTS_REQ, 3);
    p.unum32.insert(PARAM_IGNORE_CHECKSUM, 1);
    p.unum32.insert(PARAM_COLLISION_TEST_MODE, 0);
    p.bytes.insert(PARAM_CHANGE_SPEED_MSG, vec![]);
    p
}

fn sae_j2610_on_sae_j2610_sci() -> ComParamSet {
    let mut p = ComParamSet::default();
    p.unum32.insert(PARAM_CHANGE_SPEED_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RATE, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_RES_CTRL, 0);
    p.unum32.insert(PARAM_CHANGE_SPEED_TX_DELAY, 0);
    p.unum32.insert(PARAM_CYCLIC_RESP_TIMEOUT, 0);
    p.unum32.insert(ComParamId(j2534_0404::LOOPBACK), 0);
    p.unum32.insert(PARAM_REPEAT_REQ_COUNT_APP, 0);
    p.unum32.insert(PARAM_START_MSG_IND_ENABLE, 0);
    p.unum32.insert(PARAM_TRANSMIT_IND_ENABLE, 0);
    p.unum32.insert(PARAM_SCI_TRANSMIT_MODE, 0);
    // SCI hardware T timers (forwarded to J2534 adapter)
    p.unum32.insert(ComParamId(j2534_0404::T1_MAX), 20_000);
    p.unum32.insert(ComParamId(j2534_0404::T2_MAX), 100_000);
    p.unum32.insert(ComParamId(j2534_0404::T3_MAX), 50_000);
    p.unum32.insert(ComParamId(j2534_0404::T4_MAX), 20_000);
    p.unum32.insert(ComParamId(j2534_0404::T5_MAX), 100_000);
    p.unum32.insert(PARAM_SCI_SET_PROG_VOLTAGE, 0xFFFF_FFFF);
    p.unum32.insert(PARAM_SCI_ECU_SIMULATOR, 0);
    p.bytes.insert(PARAM_CHANGE_SPEED_MSG, vec![]);
    p
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_11898_2_dwcan_has_expected_defaults() {
        let p = bustype_default_params("ISO_11898_2_DWCAN").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 500_000);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::BIT_SAMPLE_POINT)], 80);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::SYNC_JUMP_WIDTH)], 15);
        assert_eq!(p.unum32[&PARAM_LISTEN_ONLY], 0);
        assert_eq!(p.unum32[&PARAM_SAMPLES_PER_BIT], 0);
        assert_eq!(p.unum32[&PARAM_TERMINATION_TYPE], 0);
        assert_eq!(p.unum32[&PARAM_CANFD_BAUDRATE], 0);
        assert_eq!(p.unum32[&PARAM_CANFD_BIT_SAMPLE_POINT], 80);
        assert_eq!(p.unum32[&PARAM_CANFD_SYNC_JUMP_WIDTH], 15);
        // Record: [500000, 250000] as LE u32 sequence = 8 bytes
        assert_eq!(p.bytes[&PARAM_CAN_BAUDRATE_RECORD].len(), 8);
    }

    #[test]
    fn iso_11898_3_dwftcan_has_expected_defaults() {
        let p = bustype_default_params("ISO_11898_3_DWFTCAN").unwrap();
        // Table B.21: CP_Baudrate default for ISO_11898_3_DWFTCAN is 125k, the
        // bus's own physical limit (ADR-130).
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 125_000);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::BIT_SAMPLE_POINT)], 80);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::SYNC_JUMP_WIDTH)], 15);
        // ISO 11898-3 FT-CAN does not define CAN FD params
        assert!(!p.unum32.contains_key(&PARAM_CANFD_BAUDRATE));
        // Table B.21's CP_CanBaudrateRecord default is only defined for
        // ISO_11898_2_DWCAN and SAE_J1939_11_DWCAN; FT-CAN has none (ADR-130).
        assert!(!p.bytes.contains_key(&PARAM_CAN_BAUDRATE_RECORD));
    }

    #[test]
    fn sae_j1939_11_dwcan_has_250kbps_default() {
        let p = bustype_default_params("SAE_J1939_11_DWCAN").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 250_000);
        assert_eq!(p.unum32[&PARAM_CANFD_BAUDRATE], 0);
        assert_eq!(p.bytes[&PARAM_CAN_BAUDRATE_RECORD].len(), 4);
    }

    /// ADR-188/Phase 7 Stage 7a, extended by ADR-192/Phase 7 Stage 7c: clause
    /// 19.3.1's fixed 500 kbps requirement, plus the shared 80%/15%
    /// sample-point/SJW default every other DW-CAN preset in this file uses.
    /// The five Stage 7a minted `PARAM_TP20_*` ComParams and Stage 7c's
    /// `PARAM_TP20_BROADCAST_ADDRESS` stay unset (no spec-mandated default /
    /// "no broadcast" by omission); `PARAM_TP20_BROADCAST_INTERVAL` is the
    /// one minted TP2.0 ComParam that does seed a default, `20` (Table 77's
    /// own `T_BR_INT` default).
    #[test]
    fn tp2_0_dwcan_has_500kbps_default_and_unset_minted_params() {
        let p = bustype_default_params("TP2_0_DWCAN").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 500_000);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::BIT_SAMPLE_POINT)], 80);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::SYNC_JUMP_WIDTH)], 15);
        assert_eq!(p.unum32[&PARAM_TP20_BROADCAST_INTERVAL], 20);
        assert!(!p.unum32.contains_key(&PARAM_TP20_CHANNEL_SETUP_CAN_ID));
        assert!(!p.unum32.contains_key(&PARAM_TP20_DESTINATION_ADDRESS));
        assert!(!p.unum32.contains_key(&PARAM_TP20_TX_ID_PROPOSAL));
        assert!(!p.unum32.contains_key(&PARAM_TP20_RX_ID_PROPOSAL));
        assert!(!p.unum32.contains_key(&PARAM_TP20_APPLICATION_TYPE));
        assert!(!p.unum32.contains_key(&PARAM_TP20_BROADCAST_ADDRESS));
    }

    /// ADR-189/Phase 8: unlike every other standalone-protocol preset above,
    /// clause 11's own Win32 API section defines no ComParam concept at all
    /// -- `DATA_RATE` is seeded at `0` (unset/unstaged), not a numeric
    /// default this project would otherwise have to invent.
    #[test]
    fn gm_uart_uart_seeds_no_non_zero_defaults() {
        let p = bustype_default_params("GM_UART_UART").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 0);
    }

    /// ADR-194/Phase 16: clause 24 defines no baud rate/loopback/timing
    /// ComParam concept at all -- the only seeded entry is
    /// `PARAM_NDIS_PIN_OPTION` at its spec-functional `0` (auto) default,
    /// case-insensitively keyed by the ISO 22900-2:2022 Table B.2-anchored
    /// bus type name `IEEE_802_3`.
    #[test]
    fn ieee_802_3_seeds_ndis_pin_option_auto_default() {
        let p = bustype_default_params("IEEE_802_3").unwrap();
        assert_eq!(p.unum32[&PARAM_NDIS_PIN_OPTION], 0);
        assert!(!p.unum32.contains_key(&ComParamId(j2534_0404::DATA_RATE)));
        assert!(bustype_default_params("ieee_802_3").is_some());
    }

    #[test]
    fn sae_j2411_swcan_has_33333bps_default() {
        let p = bustype_default_params("SAE_J2411_SWCAN").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 33_333);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::BIT_SAMPLE_POINT)], 87);
        // No CAN FD params
        assert!(!p.unum32.contains_key(&PARAM_CANFD_BAUDRATE));
    }

    #[test]
    fn iso_14230_1_uart_has_10400bps_default() {
        let p = bustype_default_params("ISO_14230_1_UART").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 10_400);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_BITS)], 6);
        assert_eq!(p.unum32[&PARAM_K_L_LINE_INIT], 0);
        assert_eq!(p.unum32[&PARAM_K_LINE_PULLUP], 0);
    }

    #[test]
    fn iso_9141_2_uart_same_as_iso14230() {
        let a = bustype_default_params("ISO_9141_2_UART").unwrap();
        let b = bustype_default_params("ISO_14230_1_UART").unwrap();
        assert_eq!(a.unum32, b.unum32);
    }

    #[test]
    fn sae_j1708_uart_has_9600bps_default() {
        let p = bustype_default_params("SAE_J1708_UART").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 9_600);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_BITS)], 6);
        // Codex review finding, PR #64: clause 17.4.5's own text gives
        // CP_MessagePriority a default of 8, matching
        // ComParamSet::msg_priority_tx_flags's own absent-value clamp.
        assert_eq!(p.unum32[&PARAM_MESSAGE_PRIORITY], 8);
    }

    #[test]
    fn sae_j2610_uart_has_7812bps_default() {
        let p = bustype_default_params("SAE_J2610_UART").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 7_812);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_BITS)], 6);
    }

    /// ADR-174/Phase 10 (Codex review finding, PR #63): SAE J2534-2 clause
    /// 13.3.1's own text -- HONDA_DIAGH_UART reuses ISO9141's own defined
    /// timing parameters, and seeds an internal `DATA_RATE` of 9600 bps so
    /// `ComParamSet::baud_rate()` (consulted by `PassThruConnect`) resolves
    /// correctly, even though that same value stays unreachable via
    /// client-facing `SetComParam`/`GetComParam`
    /// (`comparam_allowlist_excludes_data_rate_and_init_settings`,
    /// `comparam_support.rs`, confirms the client-facing half).
    #[test]
    fn honda_diagh_uart_reuses_iso9141_timing_and_seeds_internal_data_rate() {
        let p = bustype_default_params("HONDA_DIAGH_UART").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 9_600);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::LOOPBACK)], 0);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::P1_MAX)], 20_000);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::P3_MIN)], 55_000);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::P4_MIN)], 5_000);
    }

    #[test]
    fn sae_j1850_pwm_has_41600bps_default() {
        let p = bustype_default_params("SAE_J1850_PWM").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 41_600);
        assert_eq!(p.unum32[&ComParamId(j2534_0404::NETWORK_LINE)], 0);
        assert_eq!(p.unum32[&PARAM_J1850_IFR_CTRL], 1);
    }

    #[test]
    fn sae_j1850_vpw_has_10400bps_default() {
        let p = bustype_default_params("SAE_J1850_VPW").unwrap();
        assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 10_400);
        assert_eq!(p.unum32[&PARAM_J1850_IFR_CTRL], 0);
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert!(bustype_default_params("iso_11898_2_dwcan").is_some());
        assert!(bustype_default_params("ISO-11898-2-DWCAN").is_some());
    }

    /// ADR-069: the combined K-line bus type (resource 0x0212/0x0213/0x0214)
    /// connects via ISO9141 and must get that bus type's defaults (including
    /// a nonzero DATA_RATE), not an empty Working set.
    #[test]
    fn combined_kline_bustype_same_as_iso_9141_2_uart() {
        let a = bustype_default_params("ISO_9141_2_UART_and_ISO_14230_1_UART").unwrap();
        let b = bustype_default_params("ISO_9141_2_UART").unwrap();
        assert_eq!(a.unum32, b.unum32);
        assert_eq!(a.unum32[&ComParamId(j2534_0404::DATA_RATE)], 10_400);
    }

    /// ADR-069/ADR-070: the auto-detecting `SAE_J1850` bus type (resources
    /// 0x021A/0x021C/0x021D, renamed from `SAE_J1850_VPW_and_SAE_J1850_PWM`)
    /// is seeded with the VPW bus type's defaults, matching the VPW-first
    /// probe bias at `ConnectComLogicalLink`.
    #[test]
    fn sae_j1850_bustype_defaults_to_vpw_preset() {
        let a = bustype_default_params("SAE_J1850").unwrap();
        let b = bustype_default_params("SAE_J1850_VPW").unwrap();
        assert_eq!(a.unum32, b.unum32);
        assert_eq!(a.unum32[&ComParamId(j2534_0404::DATA_RATE)], 10_400);
    }

    /// ADR-070: on a PWM win, the auto-detect probe overwrites the CLL's
    /// Working set with `sae_j1850_pwm_override_params`'s result -- confirm
    /// it carries the PWM bus preset's DATA_RATE for both bus-agnostic
    /// protocols, and `None` for anything else.
    #[test]
    fn sae_j1850_pwm_override_params_carries_the_pwm_data_rate() {
        for protocol in [
            ChannelProtocol::SAE_J2190_ON_SAE_J1850,
            ChannelProtocol::ISO_15031_5_ON_SAE_J1850,
        ] {
            let p = sae_j1850_pwm_override_params(protocol).unwrap();
            assert_eq!(p.unum32[&ComParamId(j2534_0404::DATA_RATE)], 41_600);
            assert_eq!(p.unum32[&ComParamId(j2534_0404::NETWORK_LINE)], 0);
            assert_eq!(p.unum32[&PARAM_J1850_IFR_CTRL], 1);
        }
        assert!(sae_j1850_pwm_override_params(ChannelProtocol::J1850VPW).is_none());
        assert!(
            sae_j1850_pwm_override_params(ChannelProtocol::SAE_J2190_ON_SAE_J1850_VPW).is_none()
        );

        // SAE_J2190_on_SAE_J1850's PWM preset genuinely differs from its VPW
        // preset in the functional-request priority byte (0x61 PWM vs. 0x68
        // VPW) -- confirm the override carries the PWM value.
        let j2190_pwm =
            sae_j1850_pwm_override_params(ChannelProtocol::SAE_J2190_ON_SAE_J1850).unwrap();
        assert_eq!(j2190_pwm.unum32[&PARAM_FUNC_REQ_FORMAT_PRIORITY], 0x61);
    }

    /// P1 verification-pass fix: `iso_15031_5_on_sae_j1850_pwm` previously
    /// carried the VPW header/priority byte values verbatim (a copy-paste
    /// bug), so this key was absent from `sae_j1850_pwm_override_params`'s
    /// diff and a PWM-detected `ISO_15031_5_on_SAE_J1850` link kept sending
    /// VPW-formatted (`0x68`) functional OBD requests that PWM ECUs ignore.
    /// Confirm the PWM preset itself now carries the PWM byte (matching
    /// `rpc_link::J1850_OBD_PROBE_PWM`'s probe constant and
    /// `sae_j2190_on_sae_j1850_pwm`'s already-correct value), and that the
    /// override diff actually surfaces it.
    #[test]
    fn iso_15031_5_on_sae_j1850_pwm_carries_pwm_header_bytes_not_vpw() {
        let pwm = iso_15031_5_on_sae_j1850_pwm();
        assert_eq!(pwm.unum32[&PARAM_FUNC_REQ_FORMAT_PRIORITY], 0x61);
        assert_eq!(pwm.unum32[&PARAM_FUNC_RESP_FORMAT_PRIORITY], 0x41);
        assert_eq!(pwm.unum32[&PARAM_PHYS_REQ_FORMAT_PRIORITY], 0xC4);
        assert_eq!(pwm.unum32[&PARAM_PHYS_RESP_FORMAT_PRIORITY], 0xC4);

        let vpw = iso_15031_5_on_sae_j1850_vpw();
        assert_eq!(vpw.unum32[&PARAM_FUNC_REQ_FORMAT_PRIORITY], 0x68);
        assert_ne!(
            pwm.unum32[&PARAM_FUNC_REQ_FORMAT_PRIORITY],
            vpw.unum32[&PARAM_FUNC_REQ_FORMAT_PRIORITY],
            "the PWM and VPW functional-request format bytes must differ"
        );

        let override_params =
            sae_j1850_pwm_override_params(ChannelProtocol::ISO_15031_5_ON_SAE_J1850).unwrap();
        assert_eq!(
            override_params.unum32[&PARAM_FUNC_REQ_FORMAT_PRIORITY], 0x61,
            "the PWM auto-detect override must carry the PWM functional-request format byte"
        );
        assert_eq!(
            override_params.unum32[&PARAM_PHYS_REQ_FORMAT_PRIORITY],
            0xC4
        );
    }

    /// Verification-pass fix: `sae_j1850_pwm_override_params` must omit any
    /// key whose value is identical in the merged VPW and PWM presets, so a
    /// client-staged Working value for that same key (e.g. `CP_P2Max`,
    /// `j1850_common`'s `P2_MAX` at `100_000` for both flavors) survives a
    /// PWM win instead of being unconditionally clobbered.
    #[test]
    fn sae_j1850_pwm_override_params_omits_flavor_independent_keys() {
        for protocol in [
            ChannelProtocol::SAE_J2190_ON_SAE_J1850,
            ChannelProtocol::ISO_15031_5_ON_SAE_J1850,
        ] {
            let p = sae_j1850_pwm_override_params(protocol).unwrap();
            assert!(
                !p.unum32.contains_key(&ComParamId(j2534_0404::P2_MAX)),
                "P2_MAX is identical (100_000) in both the VPW and PWM presets for {protocol:?} \
                 and must not be part of the override, so a client-staged override survives"
            );
        }
    }

    #[test]
    fn unknown_bustype_returns_none() {
        assert!(bustype_default_params("UNKNOWN_BUS").is_none());
        assert!(bustype_default_params("").is_none());
    }

    #[test]
    fn baudrate_record_encoding_iso_11898_2_dwcan() {
        let p = bustype_default_params("ISO_11898_2_DWCAN").unwrap();
        let bytes = &p.bytes[&PARAM_CAN_BAUDRATE_RECORD];
        // [500000, 250000] as 2 × LE u32 (max_count and actual_count stripped)
        assert_eq!(&bytes[0..4], &500_000u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &250_000u32.to_le_bytes());
    }

    #[test]
    fn baudrate_record_encoding_j1939_dwcan() {
        let p = bustype_default_params("SAE_J1939_11_DWCAN").unwrap();
        let bytes = &p.bytes[&PARAM_CAN_BAUDRATE_RECORD];
        // [250000] as 1 × LE u32 (max_count and actual_count stripped)
        assert_eq!(&bytes[0..4], &250_000u32.to_le_bytes());
    }

    /// ADR-102: `protocol_default_params` no longer seeds `CP_P2Star` from
    /// each preset's `CP_RC78CompletionTimeout` value -- the two ComParams
    /// now play independent roles (per-occurrence reload vs. total
    /// ceiling), so the sync is removed. `CP_RC78CompletionTimeout` keeps
    /// its preset-authored value; `CP_P2Star` is simply absent from the
    /// preset's output (falling back to `RcHandlingConfig::from_params`'s
    /// own default at runtime).
    #[test]
    fn protocol_default_params_no_longer_syncs_p2_star_from_rc78_completion_timeout() {
        let p = protocol_default_params("iso_15031_5_on_iso_15765_4").unwrap();
        assert_eq!(p.unum32[&PARAM_RC78_COMPLETION_TIMEOUT], 30_000_000);
        assert!(!p.unum32.contains_key(&PARAM_P2_STAR));
    }

    /// ADR-181: the native `W4` slot is `CP_W4Min`'s own register (ISO
    /// 22900-2:2009(E) Table A.3), so it must seed `CP_W4Min`'s own
    /// 25_000us ISO default, not `CP_W4Max`'s -- both K-line
    /// (`kwp_on_kline_common`) and 9141-only (`kwp_on_9141_common`) protocol
    /// presets share this fix. The four new project-minted ids
    /// (`PARAM_W1_MIN`/`_W2_MIN`/`_W3_MIN`/`_W4_MAX`) are seeded alongside
    /// it with their own independent ISO defaults, distinct from the native
    /// W1-W4 slots they used to silently collide with.
    #[test]
    fn kwp_presets_seed_corrected_w4_and_the_four_new_min_max_project_ids() {
        for name in [
            "iso_14230_3_on_iso_14230_2",
            "sae_j2190_on_iso_14230_2",
            "iso_15031_5_on_iso_14230_4",
            "iso_15031_5_on_iso_9141_2",
            "sae_j2190_on_iso_9141_2",
        ] {
            let p = protocol_default_params(name).unwrap();
            assert_eq!(
                p.unum32[&ComParamId(j2534_0404::W4)],
                25_000,
                "{name}: native W4 slot should be CP_W4Min's corrected default"
            );
            assert_eq!(p.unum32[&PARAM_W1_MIN], 60_000, "{name}: CP_W1Min");
            assert_eq!(p.unum32[&PARAM_W2_MIN], 5_000, "{name}: CP_W2Min");
            assert_eq!(p.unum32[&PARAM_W3_MIN], 0, "{name}: CP_W3Min");
            assert_eq!(p.unum32[&PARAM_W4_MAX], 50_000, "{name}: CP_W4Max");
            // W1-W3's own Max-side defaults are unchanged by this fix.
            assert_eq!(p.unum32[&ComParamId(j2534_0404::W1)], 300_000, "{name}: W1");
            assert_eq!(p.unum32[&ComParamId(j2534_0404::W2)], 20_000, "{name}: W2");
            assert_eq!(p.unum32[&ComParamId(j2534_0404::W3)], 20_000, "{name}: W3");
        }
    }

    /// ADR-069: the four combined/alias resource protocol names not covered
    /// by an existing preset (resources 0x0212, 0x0214, 0x021C) and the
    /// per-configuration SCI resources (0x021F-0x0222) must resolve to a
    /// non-empty preset, parallel to their closest existing counterpart.
    #[test]
    fn combined_and_sci_protocol_names_resolve_to_expected_presets() {
        assert_eq!(
            protocol_default_params("iso_15031_5_on_iso_9141_2_and_iso_14230_4")
                .unwrap()
                .unum32,
            protocol_default_params("iso_15031_5_on_iso_9141_2")
                .unwrap()
                .unum32
        );
        assert_eq!(
            protocol_default_params("sae_j2190_on_iso_9141_2_and_iso_14230_2")
                .unwrap()
                .unum32,
            protocol_default_params("sae_j2190_on_iso_9141_2")
                .unwrap()
                .unum32
        );
        assert_eq!(
            protocol_default_params("iso_15031_5_on_sae_j1850")
                .unwrap()
                .unum32,
            protocol_default_params("iso_15031_5_on_sae_j1850_vpw")
                .unwrap()
                .unum32
        );
        assert_eq!(
            protocol_default_params("sae_j2190_on_sae_j1850")
                .unwrap()
                .unum32,
            protocol_default_params("sae_j2190_on_sae_j1850_vpw")
                .unwrap()
                .unum32
        );
        assert_eq!(
            protocol_default_params("sae_j2610_sci").unwrap().unum32,
            protocol_default_params("sae_j2610_on_sae_j2610_sci")
                .unwrap()
                .unum32
        );
    }

    /// ADR-138: Table B.20 defines `CP_TesterPresentAddrMode = 0` (physical)
    /// for every protocol identity except `ISO_15765_3`. Four presets had
    /// been seeded `1` (apparently copied from those same presets'
    /// `CP_RequestAddrMode = 2`, which the spec does not mirror for this
    /// ComParam) and are corrected here.
    #[test]
    fn tester_present_addr_mode_presets_match_table_b20_defaults() {
        for name in [
            "iso_15031_5_on_iso_14230_4",
            "iso_15031_5_on_iso_9141_2",
            "iso_15031_5_on_iso_9141_2_and_iso_14230_4",
            "iso_obd_on_k_line",
            "iso_15031_5_on_iso_15765_4",
            "iso_obd_on_iso_15765_4",
        ] {
            let p = protocol_default_params(name).unwrap();
            assert_eq!(
                p.unum32[&PARAM_TESTER_PRESENT_ADDR_MODE], 0,
                "{name} should default CP_TesterPresentAddrMode to 0 (physical)"
            );
        }
        // ISO_15765_3 is the sole spec-functional default (Table B.20) and
        // must remain unchanged.
        let iso_15765_3 = protocol_default_params("iso_15765_3_on_iso_15765_2").unwrap();
        assert_eq!(iso_15765_3.unum32[&PARAM_TESTER_PRESENT_ADDR_MODE], 1);
    }
}
