//! ABI-independent J2534 v04.04 definitions shared by the worker and sim-vci.
//!
//! - [`consts`]: numeric constants (status codes, protocol IDs, flags, config parameter IDs, IOCTL IDs)
//! - [`protocol_name`]: ISO 22900-2 / J2534 protocol names -> J2534 `ProtocolID`
//! - [`comparam`]: COMPARAM mapping from ISO 22900-2 `CP_*` to J2534 `SET_CONFIG` (8.5)
//!
//! Only values are defined here, never structure layouts or function signatures:
//! those depend on the width of `unsigned long` and the calling convention,
//! which are decided at run time from the ABI table (7.1.2).
//!
//! Values follow `j2534-0404-sys` and the name/ComParam tables of `j2534-0404-service`.

pub mod comparam;
pub mod consts;
pub mod protocol_name;

/// Symbolic name of a J2534 status code, for logs and error reports.
/// Falls back to the hex value for unknown codes (vendor-specific codes exist).
pub fn status_name(status: u32) -> std::borrow::Cow<'static, str> {
    match consts::status::name(status) {
        Some(name) => name.into(),
        None => format!("{status:#010x}").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_name_known_and_unknown() {
        assert_eq!(status_name(consts::status::ERR_TIMEOUT), "ERR_TIMEOUT");
        assert_eq!(status_name(0x1234_5678), "0x12345678");
    }

    #[test]
    fn name_tables_round_trip() {
        assert_eq!(consts::protocol::name(0x06), Some("PROTOCOL_ISO15765"));
        assert_eq!(
            consts::config::name(consts::config::CONFIG_STMIN_TX),
            Some("CONFIG_STMIN_TX")
        );
        assert_eq!(
            consts::ioctl::name(consts::ioctl::IOCTL_READ_VBATT),
            Some("IOCTL_READ_VBATT")
        );
    }
}
