//! Resolution of protocol names to J2534 `ProtocolID`.
//!
//! ODX/MDF data names protocols with ISO 22900-2 names (Annex B.1.5 short names such as
//! `ISO_15765_3_on_ISO_15765_2`, or plain names such as `ISO15765`), while J2534 only knows
//! numeric `ProtocolID`s. Matching is case-insensitive, following ISO 22900-2's naming guidelines.
//!
//! Only names that map unambiguously onto a J2534-1 (v04.04) base protocol are listed.
//! Protocols J2534-1 does not define (J1939, J1708, DoIP, ...) are deliberately absent.
//!
//! Derived from `crates/j2534-0404-service/docs/protocol-mapping.md`.

use crate::consts::protocol::*;

/// (name, ProtocolID). Names are compared case-insensitively.
pub const PROTOCOL_NAMES: &[(&str, u32)] = &[
    // J2534 names, ISO 22900-2 plain names and common aliases
    ("J1850VPW", PROTOCOL_J1850VPW),
    ("VPW", PROTOCOL_J1850VPW),
    ("J1850PWM", PROTOCOL_J1850PWM),
    ("PWM", PROTOCOL_J1850PWM),
    ("ISO9141", PROTOCOL_ISO9141),
    ("ISO9141-2", PROTOCOL_ISO9141),
    ("KWP", PROTOCOL_ISO9141),
    ("ISO14230", PROTOCOL_ISO14230),
    ("ISO14230-1", PROTOCOL_ISO14230),
    ("KWP2000", PROTOCOL_ISO14230),
    ("CAN", PROTOCOL_CAN),
    ("ISO11898", PROTOCOL_CAN),
    ("ISO11898-1", PROTOCOL_CAN),
    ("ISO15765", PROTOCOL_ISO15765),
    ("ISO15765-2", PROTOCOL_ISO15765),
    ("ISO-TP", PROTOCOL_ISO15765),
    ("SCI_A_ENGINE", PROTOCOL_SCI_A_ENGINE),
    ("SCI_A_TRANS", PROTOCOL_SCI_A_TRANS),
    ("SCI_B_ENGINE", PROTOCOL_SCI_B_ENGINE),
    ("SCI_B_TRANS", PROTOCOL_SCI_B_TRANS),
    // ISO 22900-2 Annex B.1.5 short names (application layer _on_ transport layer)
    ("ISO_14230_3_on_ISO_14230_2", PROTOCOL_ISO14230),
    ("ISO_14230_3_on_ISO_15765_2", PROTOCOL_ISO15765),
    ("ISO_15765_3_on_ISO_15765_2", PROTOCOL_ISO15765),
    ("ISO_14229_3_on_ISO_15765_2", PROTOCOL_ISO15765),
    ("SAE_J2190_on_ISO_14230_2", PROTOCOL_ISO14230),
    ("SAE_J2190_on_ISO_9141_2", PROTOCOL_ISO9141),
    ("SAE_J2190_on_ISO_15765_2", PROTOCOL_ISO15765),
    ("SAE_J2190_on_SAE_J1850_VPW", PROTOCOL_J1850VPW),
    ("SAE_J2190_on_SAE_J1850_PWM", PROTOCOL_J1850PWM),
    ("ISO_15031_5_on_ISO_9141_2", PROTOCOL_ISO9141),
    ("ISO_15031_5_on_SAE_J1850_VPW", PROTOCOL_J1850VPW),
    ("ISO_15031_5_on_ISO_15765_4", PROTOCOL_ISO15765),
    ("ISO_15031_5_on_SAE_J1850_PWM", PROTOCOL_J1850PWM),
    ("ISO_15031_5_on_ISO_14230_4", PROTOCOL_ISO14230),
    ("ISO_11898_RAW", PROTOCOL_CAN),
    ("ISO_11783_12_on_ISO_11783_5", PROTOCOL_CAN),
    (
        "ISO_14229_3_on_ISO_15765_2_with_ISO_11783_5",
        PROTOCOL_ISO15765,
    ),
];

/// Resolves a protocol name to a J2534 `ProtocolID` (case-insensitive).
pub fn resolve(name: &str) -> Option<u32> {
    let name = name.trim();
    PROTOCOL_NAMES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|&(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_case_insensitively() {
        assert_eq!(
            resolve("iso_15765_3_on_iso_15765_2"),
            Some(PROTOCOL_ISO15765)
        );
        assert_eq!(resolve(" kwp2000 "), Some(PROTOCOL_ISO14230));
        assert_eq!(resolve("SAE_J1939_73_on_SAE_J1939_21"), None);
    }

    #[test]
    fn names_are_unique() {
        for (i, (a, _)) in PROTOCOL_NAMES.iter().enumerate() {
            for (b, _) in &PROTOCOL_NAMES[i + 1..] {
                assert!(!a.eq_ignore_ascii_case(b), "duplicate name {a}");
            }
        }
    }
}
