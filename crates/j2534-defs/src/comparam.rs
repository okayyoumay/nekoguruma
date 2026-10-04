//! COMPARAM mapping (8.5): ISO 22900-2 `CP_*` ComParams -> J2534 `SET_CONFIG` parameters.
//!
//! ODX COMPARAMs are defined with the D-PDU API in mind. When a link runs over J2534, each
//! COMPARAM value is written with `PassThruIoctl(SET_CONFIG)` to the J2534 parameter listed
//! here, after unit conversion. The table is data so that it can be reviewed and extended in
//! one place, matching the ABI table policy (7.1.2).
//!
//! Scope: J2534-1 (v04.04) base protocols only. A COMPARAM that is not in the table, or not
//! listed for the link's protocol, has no J2534-1 equivalent and must not be forwarded.
//! This includes `CP_P1Min`, `CP_P2Min`, `CP_P2Max`, `CP_P3Max_Ecu` and `CP_P4Max`: J2534-1
//! defines the parameters `P1_MIN`, `P2_*`, `P3_MAX` and `P4_MAX` but supports them for no
//! protocol. J2534-2 additions (CAN FD, J1939, TP2.0, ...) are out of scope.
//!
//! Derived from `j2534-0404-service` (`comparam_id.rs`, `docs/comparam-mapping.md`), keeping
//! only the ComParam names defined by ISO 22900-2.

use crate::consts::config::*;
use crate::consts::protocol::*;

/// Protocols a mapping applies to (by J2534-1 base `ProtocolID`, see [`base_protocol_id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocols {
    All,
    J1850Pwm,
    /// ISO9141 and ISO14230.
    KLine,
    Iso9141,
    Iso14230,
    Can,
    Iso15765,
    /// SCI_A_ENGINE, SCI_A_TRANS, SCI_B_ENGINE, SCI_B_TRANS.
    Sci,
}

impl Protocols {
    pub fn contains(self, protocol_id: u32) -> bool {
        let p = base_protocol_id(protocol_id);
        match self {
            Protocols::All => true,
            Protocols::J1850Pwm => p == PROTOCOL_J1850PWM,
            Protocols::KLine => p == PROTOCOL_ISO9141 || p == PROTOCOL_ISO14230,
            Protocols::Iso9141 => p == PROTOCOL_ISO9141,
            Protocols::Iso14230 => p == PROTOCOL_ISO14230,
            Protocols::Can => p == PROTOCOL_CAN,
            Protocols::Iso15765 => p == PROTOCOL_ISO15765,
            Protocols::Sci => matches!(
                p,
                PROTOCOL_SCI_A_ENGINE
                    | PROTOCOL_SCI_A_TRANS
                    | PROTOCOL_SCI_B_ENGINE
                    | PROTOCOL_SCI_B_TRANS
            ),
        }
    }
}

/// Value conversion from COMPARAM units to J2534 units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conversion {
    /// Same units and encoding on both sides.
    Identity,
    /// 1 us -> 0.5 ms, rounded to nearest, clamped to `min..=0xFFFF`.
    UsToHalfMs { min: u32 },
    /// 1 us -> 1 ms, rounded to nearest, clamped to `0..=0xFFFF`.
    UsToMs,
    /// `CP_StMinOverride` (us, `0xFFFFFFFF` = use the vehicle's value) -> `STMIN_TX`
    /// (ISO 15765-2 STmin byte, `0xFFFF` = use the vehicle's value). See [`us_to_stmin_tx`].
    UsToStMinTx,
    /// `CP_UartConfig` -> `DATA_BITS` (see [`uart_config`]).
    UartDataBits,
    /// `CP_UartConfig` -> `PARITY` (see [`uart_config`]).
    UartParity,
}

impl Conversion {
    /// Converts a COMPARAM value. `None` means the value cannot be represented in J2534-1
    /// and the link should be rejected rather than silently approximated.
    pub fn apply(self, value: u32) -> Option<u32> {
        Some(match self {
            Conversion::Identity => value,
            Conversion::UsToHalfMs { min } => (value.saturating_add(250) / 500).clamp(min, 0xFFFF),
            Conversion::UsToMs => (value.saturating_add(500) / 1000).min(0xFFFF),
            Conversion::UsToStMinTx => us_to_stmin_tx(value),
            Conversion::UartDataBits => uart_config(value)?.0,
            Conversion::UartParity => uart_config(value)?.1,
        })
    }
}

/// One row of the mapping table.
#[derive(Debug, Clone, Copy)]
pub struct Mapping {
    pub comparam: &'static str,
    pub config_id: u32,
    pub protocols: Protocols,
    pub conversion: Conversion,
    /// The J2534 parameter is the closest equivalent, not an exact one.
    pub approximate: bool,
}

const fn m(
    comparam: &'static str,
    config_id: u32,
    protocols: Protocols,
    conversion: Conversion,
) -> Mapping {
    Mapping {
        comparam,
        config_id,
        protocols,
        conversion,
        approximate: false,
    }
}

const fn approx(
    comparam: &'static str,
    config_id: u32,
    protocols: Protocols,
    conversion: Conversion,
) -> Mapping {
    Mapping {
        comparam,
        config_id,
        protocols,
        conversion,
        approximate: true,
    }
}

use Conversion::*;
use Protocols::*;

/// A COMPARAM may appear more than once: `CP_TIdle` also sets `W0` / `W5` (it replaces both
/// in ISO 22900-2), and `CP_UartConfig` sets both `DATA_BITS` and `PARITY`.
pub const MAPPINGS: &[Mapping] = &[
    m("CP_Baudrate", CONFIG_DATA_RATE, All, Identity),
    m("CP_Loopback", CONFIG_LOOPBACK, All, Identity),
    m("CP_NetworkLine", CONFIG_NETWORK_LINE, J1850Pwm, Identity),
    // K-Line timing
    m("CP_P1Max", CONFIG_P1_MAX, KLine, UsToHalfMs { min: 1 }),
    m("CP_P3Min", CONFIG_P3_MIN, KLine, UsToHalfMs { min: 0 }),
    approx("CP_P3Phys", CONFIG_P3_MIN, KLine, UsToHalfMs { min: 0 }),
    approx("CP_P3Func", CONFIG_P3_MIN, KLine, UsToHalfMs { min: 0 }),
    m("CP_P4Min", CONFIG_P4_MIN, KLine, UsToHalfMs { min: 0 }),
    approx("CP_W1Max", CONFIG_W1, KLine, UsToMs),
    approx("CP_W2Max", CONFIG_W2, KLine, UsToMs),
    approx("CP_W3Max", CONFIG_W3, KLine, UsToMs),
    approx("CP_W4Min", CONFIG_W4, KLine, UsToMs),
    m("CP_TIdle", CONFIG_TIDLE, KLine, UsToMs),
    m("CP_TIdle", CONFIG_W0, Iso9141, UsToMs),
    m("CP_TIdle", CONFIG_W5, Iso14230, UsToMs),
    approx("CP_TInil", CONFIG_TINIL, KLine, UsToMs),
    approx("CP_TWup", CONFIG_TWUP, KLine, UsToMs),
    m("CP_5BaudMode", CONFIG_FIVE_BAUD_MOD, KLine, Identity),
    m("CP_UartConfig", CONFIG_DATA_BITS, KLine, UartDataBits),
    m("CP_UartConfig", CONFIG_PARITY, KLine, UartParity),
    // CAN bit timing
    m("CP_BitSamplePoint", CONFIG_BIT_SAMPLE_POINT, Can, Identity),
    m("CP_SyncJumpWidth", CONFIG_SYNC_JUMP_WIDTH, Can, Identity),
    // SCI
    m("CP_T1Max", CONFIG_T1_MAX, Sci, UsToMs),
    m("CP_T2Max", CONFIG_T2_MAX, Sci, UsToMs),
    m("CP_T3Max", CONFIG_T3_MAX, Sci, UsToMs),
    m("CP_T4Max", CONFIG_T4_MAX, Sci, UsToMs),
    m("CP_T5Max", CONFIG_T5_MAX, Sci, UsToMs),
    // ISO 15765-2
    m("CP_BlockSize", CONFIG_ISO15765_BS, Iso15765, Identity),
    m("CP_StMin", CONFIG_ISO15765_STMIN, Iso15765, Identity),
    m("CP_BlockSizeOverride", CONFIG_BS_TX, Iso15765, Identity),
    m("CP_StMinOverride", CONFIG_STMIN_TX, Iso15765, UsToStMinTx),
    m(
        "CP_CanMaxNumWaitFrames",
        CONFIG_ISO15765_WFT_MAX,
        Iso15765,
        Identity,
    ),
];

/// The J2534 parameters a COMPARAM is written to on a link with `protocol_id`.
/// Empty if the COMPARAM has no J2534-1 equivalent for that protocol.
pub fn mappings_for(
    comparam: &str,
    protocol_id: u32,
) -> impl Iterator<Item = &'static Mapping> + '_ {
    MAPPINGS
        .iter()
        .filter(move |m| m.comparam == comparam && m.protocols.contains(protocol_id))
}

/// Normalizes a J2534-2 pin-switched (`_PS`) ProtocolID to its J2534-1 base protocol.
/// Other IDs are returned unchanged (and so match no base-protocol mapping).
pub fn base_protocol_id(protocol_id: u32) -> u32 {
    match protocol_id {
        PROTOCOL_J1850VPW_PS => PROTOCOL_J1850VPW,
        PROTOCOL_J1850PWM_PS => PROTOCOL_J1850PWM,
        PROTOCOL_ISO9141_PS => PROTOCOL_ISO9141,
        PROTOCOL_ISO14230_PS => PROTOCOL_ISO14230,
        PROTOCOL_CAN_PS => PROTOCOL_CAN,
        PROTOCOL_ISO15765_PS => PROTOCOL_ISO15765,
        other => other,
    }
}

/// Decodes `CP_UartConfig` into J2534 `(DATA_BITS, PARITY)`.
///
/// `CP_UartConfig` encodes data bits x parity x stop bits as `0..=17`. J2534-1 has no stop-bit
/// parameter (always 1) and `DATA_BITS` only knows 7 or 8 bits, so only 7N1/7O1/7E1 (`0..=2`)
/// and 8N1/8O1/8E1 (`6..=8`) are representable. `DATA_BITS`: 0 = 8 bits, 1 = 7 bits.
/// `PARITY`: 0 = none, 1 = odd, 2 = even.
pub fn uart_config(value: u32) -> Option<(u32, u32)> {
    let data_bits = match value {
        0..=2 => 1,
        6..=8 => 0,
        _ => return None,
    };
    Some((data_bits, value % 3))
}

/// Converts a separation time in microseconds into the ISO 15765-2 STmin byte used by
/// `STMIN_TX`: `0x00..=0x7F` = 0-127 ms, `0xF1..=0xF9` = 100-900 us.
/// Rounds to the nearest representable step (a tie favors the larger value) and clamps above
/// 127 ms. `0xFFFFFFFF` ("use the vehicle's value") maps to the J2534 sentinel `0xFFFF`.
pub fn us_to_stmin_tx(value_us: u32) -> u32 {
    if value_us == u32::MAX {
        return 0xFFFF;
    }
    let us_step = (value_us.saturating_add(50) / 100).clamp(1, 9);
    let ms_step = (value_us.saturating_add(500) / 1000).min(0x7F);
    let us_diff = value_us.abs_diff(us_step * 100);
    let ms_diff = value_us.abs_diff(ms_step * 1000);
    if us_diff < ms_diff || (us_diff == ms_diff && us_step * 100 > ms_step * 1000) {
        0xF0 + us_step
    } else {
        ms_step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single(comparam: &str, protocol_id: u32) -> Option<&'static Mapping> {
        let mut it = mappings_for(comparam, protocol_id);
        let first = it.next();
        assert!(it.next().is_none());
        first
    }

    #[test]
    fn protocol_gating() {
        assert!(single("CP_BlockSize", PROTOCOL_ISO15765).is_some());
        assert!(single("CP_BlockSize", PROTOCOL_CAN).is_none());
        assert!(single("CP_BlockSize", PROTOCOL_ISO15765_PS).is_some());
        assert!(single("CP_P2Max", PROTOCOL_ISO14230).is_none());
        assert!(single("CP_Baudrate", PROTOCOL_SCI_B_TRANS).is_some());
    }

    #[test]
    fn tidle_fans_out_per_kline_protocol() {
        let ids = |p| {
            mappings_for("CP_TIdle", p)
                .map(|m| m.config_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(PROTOCOL_ISO9141), [CONFIG_TIDLE, CONFIG_W0]);
        assert_eq!(ids(PROTOCOL_ISO14230), [CONFIG_TIDLE, CONFIG_W5]);
        assert!(ids(PROTOCOL_CAN).is_empty());
    }

    #[test]
    fn unit_conversions() {
        assert_eq!(UsToHalfMs { min: 1 }.apply(0), Some(1));
        assert_eq!(UsToHalfMs { min: 0 }.apply(55_000), Some(110));
        assert_eq!(UsToMs.apply(300_000), Some(300));
        assert_eq!(UsToMs.apply(u32::MAX), Some(0xFFFF));
    }

    #[test]
    fn uart_config_decoding() {
        assert_eq!(uart_config(0), Some((1, 0))); // 7N1
        assert_eq!(uart_config(8), Some((0, 2))); // 8E1
        assert_eq!(uart_config(3), None); // 7N2
        assert_eq!(UartParity.apply(12), None); // 9N1
    }

    #[test]
    fn stmin_tx_encoding() {
        assert_eq!(us_to_stmin_tx(u32::MAX), 0xFFFF);
        assert_eq!(us_to_stmin_tx(0), 0x00);
        assert_eq!(us_to_stmin_tx(120), 0xF1);
        assert_eq!(us_to_stmin_tx(500), 0xF5);
        assert_eq!(us_to_stmin_tx(1_000), 0x01);
        assert_eq!(us_to_stmin_tx(20_000), 0x14);
        assert_eq!(us_to_stmin_tx(500_000), 0x7F);
    }
}
