use crate::{
    Error, ISO15765, PassThruMessage, TX_EXTENDED_ID, TX_ISO15765_FRAME_PAD, TX_NORMAL_TRANSMIT,
};

const NORMAL_ADDRESSING_MAX_SF_PAYLOAD: usize = 7;

/// Builds a normal-addressing native-ISO15765 write for an 11-bit CAN ID.
///
/// `Data` is `[4-byte big-endian CAN ID][raw payload]`; the adapter inserts the
/// ISO-TP PCI/framing bytes transparently for the native `ISO15765` protocol, so no
/// PCI byte is prepended here. The payload is capped at
/// `NORMAL_ADDRESSING_MAX_SF_PAYLOAD` (7 bytes) not because of manual PCI framing, but
/// because SAE J2534-1 §7.2.6 only guarantees a transmit without an installed
/// flow-control filter for a SingleFrame-sized payload with normal addressing; neither
/// consumer of
/// this helper in this repo installs a flow-control filter, so this remains the
/// safe/guaranteed-transmittable boundary.
pub fn single_frame(can_id: u32, payload: &[u8]) -> Result<PassThruMessage, Error> {
    if payload.is_empty() {
        return Err(Error::InvalidArgument {
            name: "payload",
            reason: "single frame payload cannot be empty",
        });
    }

    if payload.len() > NORMAL_ADDRESSING_MAX_SF_PAYLOAD {
        return Err(Error::InvalidArgument {
            name: "payload",
            reason: "single frame payload cannot exceed 7 bytes for normal addressing",
        });
    }

    if can_id > 0x7FF {
        return Err(Error::InvalidArgument {
            name: "can_id",
            reason: "11-bit CAN id must be <= 0x7FF",
        });
    }

    let mut data = can_id.to_be_bytes().to_vec();
    data.extend_from_slice(payload);

    PassThruMessage::new(ISO15765, 0, TX_NORMAL_TRANSMIT, 0, 0, &data)
}

/// Builds a normal-addressing native-ISO15765 write for a 29-bit CAN ID.
///
/// Same `Data` layout and payload cap as [`single_frame`] (see its doc comment). This
/// helper does not support ISO15765 *extended addressing* (an in-payload addressing
/// mode, orthogonal to CAN ID width) — `TX_EXTENDED_ID` here only selects a 29-bit CAN
/// ID, so no Address Extension byte is added.
pub fn single_frame_extended(can_id: u32, payload: &[u8]) -> Result<PassThruMessage, Error> {
    if payload.is_empty() {
        return Err(Error::InvalidArgument {
            name: "payload",
            reason: "single frame payload cannot be empty",
        });
    }

    if payload.len() > NORMAL_ADDRESSING_MAX_SF_PAYLOAD {
        return Err(Error::InvalidArgument {
            name: "payload",
            reason: "single frame payload cannot exceed 7 bytes for normal addressing",
        });
    }

    if can_id > 0x1FFF_FFFF {
        return Err(Error::InvalidArgument {
            name: "can_id",
            reason: "29-bit CAN id must be <= 0x1FFFFFFF",
        });
    }

    let mut data = can_id.to_be_bytes().to_vec();
    data.extend_from_slice(payload);

    PassThruMessage::new(ISO15765, 0, TX_EXTENDED_ID, 0, 0, &data)
}

/// Enables device-side ISO15765 frame padding.
///
/// Sets `TX_ISO15765_FRAME_PAD`, which tells the device to zero-pad the transmitted
/// CAN frame(s) to full classic-CAN length on the wire; this does not touch the
/// logical `Data` buffer, so this operation cannot fail.
pub fn with_padding(mut message: PassThruMessage) -> PassThruMessage {
    message.set_tx_flags(message.tx_flags() | TX_ISO15765_FRAME_PAD);
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_single_frame_for_normal_id() {
        let msg = single_frame(0x7E0, &[0x10, 0x03]).expect("single frame should build");
        assert_eq!(msg.protocol_id(), ISO15765);
        assert_eq!(msg.tx_flags(), TX_NORMAL_TRANSMIT);
        assert_eq!(
            msg.data().expect("data should be valid"),
            &[0x00, 0x00, 0x07, 0xE0, 0x10, 0x03]
        );
    }

    #[test]
    fn rejects_empty_payload() {
        let err = single_frame(0x7E0, &[]).expect_err("empty payload must be rejected");
        match err {
            Error::InvalidArgument { name, .. } => assert_eq!(name, "payload"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn pads_message_to_classic_can_size() {
        let msg = single_frame(0x7E0, &[0x3E, 0x00]).expect("single frame should build");
        let padded = with_padding(msg);
        let data = padded.data().expect("data should be valid");
        assert_eq!(data, &[0x00, 0x00, 0x07, 0xE0, 0x3E, 0x00]);
        assert_eq!(
            padded.tx_flags() & TX_ISO15765_FRAME_PAD,
            TX_ISO15765_FRAME_PAD
        );
    }
}
