use std::mem::{offset_of, size_of};

use j2534_0404_sys::bindings::*;

#[test]
fn constants_match_v0404_core_values() {
    assert_eq!(STATUS_NOERROR, 0x0000_0000);
    assert_eq!(ERR_INVALID_CHANNEL_ID, 0x0000_0002);
    assert_eq!(ERR_INVALID_IOCTL_ID, 0x0000_000F);
    assert_eq!(PROTOCOL_CAN, 0x0000_0005);
    assert_eq!(PROTOCOL_ISO15765, 0x0000_0006);
    assert_eq!(CONFIG_DATA_RATE, 0x0000_0001);
    assert_eq!(IOCTL_CLEAR_RX_BUFFER, 0x0000_0008);
    assert_eq!(IOCTL_CLEAR_MSG_FILTERS, 0x0000_000A);
    assert_eq!(IOCTL_FIVE_BAUD_INIT, 0x0000_0004);
    assert_eq!(IOCTL_FAST_INIT, 0x0000_0005);
}

#[test]
fn passthru_msg_layout_matches_native_abi() {
    assert_eq!(size_of::<PASSTHRU_MSG>(), 4152);
    assert_eq!(offset_of!(PASSTHRU_MSG, ProtocolID), 0);
    assert_eq!(offset_of!(PASSTHRU_MSG, RxStatus), 4);
    assert_eq!(offset_of!(PASSTHRU_MSG, TxFlags), 8);
    assert_eq!(offset_of!(PASSTHRU_MSG, Timestamp), 12);
    assert_eq!(offset_of!(PASSTHRU_MSG, DataSize), 16);
    assert_eq!(offset_of!(PASSTHRU_MSG, ExtraDataIndex), 20);
    assert_eq!(offset_of!(PASSTHRU_MSG, Data), 24);
}

#[test]
fn config_struct_layout_matches_native_abi() {
    assert_eq!(size_of::<SCONFIG>(), 8);
    assert_eq!(offset_of!(SCONFIG, Parameter), 0);
    assert_eq!(offset_of!(SCONFIG, Value), 4);

    assert_eq!(offset_of!(SCONFIG_LIST, NumOfParams), 0);
    assert_eq!(offset_of!(SBYTE_ARRAY, NumOfBytes), 0);
}
