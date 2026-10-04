//! Verifies, per J2534 protocol, that communication parameters set through
//! `PassThruIoctl(SET_CONFIG)` are stored correctly and that transmit data
//! built with `j2534-0404`'s message/ISO-TP helpers reaches the mock DLL
//! unchanged (protocol ID, TX flags, and payload bytes).
//!
//! Tests call `j2534-0404-mock`'s exported `PassThru*` functions directly
//! in-process (no dynamic library loading), so the crate's Rust-side
//! back-door accessors (`mock_get_config_value`, `mock_get_written_messages`)
//! observe the same state the calls mutate.

use std::os::raw::{c_long, c_void};

use j2534_0404::{
    BIT_SAMPLE_POINT, BS_TX, CAN, DATA_RATE, ISO9141, ISO14230, ISO15765, ISO15765_BS,
    ISO15765_STMIN, J1850PWM, J1850VPW, NETWORK_LINE, NODE_ADDRESS, P1_MAX, P1_MIN, P2_MAX, P2_MIN,
    P3_MAX, P3_MIN, PARITY, PassThruMessage, STMIN_TX, SYNC_JUMP_WIDTH, TIDLE, TINIL, TWUP,
    TX_EXTENDED_ID, TX_ISO15765_FRAME_PAD, TX_NORMAL_TRANSMIT, W1, W2, W3, W4, W5, iso15765,
};
use j2534_0404_mock::{
    PassThruConnect, PassThruIoctl, PassThruOpen, PassThruWriteMsgs, mock_get_config_value,
    mock_get_written_messages,
};
use j2534_0404_sys::bindings::{
    IOCTL_SET_CONFIG, PASSTHRU_MSG, SCONFIG, SCONFIG_LIST, STATUS_NOERROR,
};

fn connect(protocol_id: u32, flags: u32, baud_rate: u32) -> u32 {
    let mut device_id = 0u32;
    let rc = unsafe { PassThruOpen(std::ptr::null_mut(), &mut device_id) };
    assert_eq!(rc, STATUS_NOERROR as c_long);

    let mut channel_id = 0u32;
    let rc = unsafe { PassThruConnect(device_id, protocol_id, flags, baud_rate, &mut channel_id) };
    assert_eq!(rc, STATUS_NOERROR as c_long);
    channel_id
}

fn set_config(channel_id: u32, params: &[(u32, u32)]) {
    let mut configs: Vec<SCONFIG> = params
        .iter()
        .map(|&(parameter, value)| SCONFIG {
            Parameter: parameter,
            Value: value,
        })
        .collect();
    let mut list = SCONFIG_LIST {
        NumOfParams: configs.len() as u32,
        ConfigPtr: configs.as_mut_ptr(),
    };
    let rc = unsafe {
        PassThruIoctl(
            channel_id,
            IOCTL_SET_CONFIG,
            &mut list as *mut _ as *mut c_void,
            std::ptr::null_mut(),
        )
    };
    assert_eq!(rc, STATUS_NOERROR as c_long);
}

fn assert_config(channel_id: u32, params: &[(u32, u32)]) {
    for &(parameter, expected) in params {
        assert_eq!(
            mock_get_config_value(channel_id, parameter),
            Some(expected),
            "parameter {parameter:#x} should round-trip through SET_CONFIG"
        );
    }
}

fn write_message(channel_id: u32, message: &mut PassThruMessage) {
    let mut num_messages = 1u32;
    let rc = unsafe {
        PassThruWriteMsgs(
            channel_id,
            &mut message.0 as *mut PASSTHRU_MSG,
            &mut num_messages,
            1000,
        )
    };
    assert_eq!(rc, STATUS_NOERROR as c_long);
    assert_eq!(num_messages, 1);
}

#[test]
fn can_protocol_sets_bit_timing_and_writes_extended_id_frame() {
    let channel_id = connect(CAN, 0, 500_000);

    let params = [
        (DATA_RATE, 500_000),
        (BIT_SAMPLE_POINT, 80),
        (SYNC_JUMP_WIDTH, 15),
    ];
    set_config(channel_id, &params);
    assert_config(channel_id, &params);

    // Raw CAN frame: 4-byte 29-bit arbitration ID header followed by payload.
    let mut data = 0x18DAF110u32.to_be_bytes().to_vec();
    data.extend_from_slice(&[0x02, 0x10, 0x03]);
    let mut message =
        PassThruMessage::new(CAN, 0, TX_EXTENDED_ID, 0, 0, &data).expect("CAN frame should build");

    write_message(channel_id, &mut message);

    let written = mock_get_written_messages(channel_id);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].protocol_id, CAN);
    assert_eq!(written[0].tx_flags, TX_EXTENDED_ID);
    assert_eq!(written[0].data, data);
}

#[test]
fn iso15765_protocol_sets_flow_control_params_and_writes_single_frame() {
    let channel_id = connect(ISO15765, 0, 500_000);

    let params = [
        (DATA_RATE, 500_000),
        (ISO15765_BS, 0),
        (ISO15765_STMIN, 0),
        (BS_TX, 8),
        (STMIN_TX, 10),
    ];
    set_config(channel_id, &params);
    assert_config(channel_id, &params);

    let mut message =
        iso15765::single_frame(0x7E0, &[0x10, 0x03]).expect("single frame should build");

    write_message(channel_id, &mut message);

    let written = mock_get_written_messages(channel_id);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].protocol_id, ISO15765);
    assert_eq!(written[0].tx_flags, TX_NORMAL_TRANSMIT);
    assert_eq!(written[0].data, vec![0x00, 0x00, 0x07, 0xE0, 0x10, 0x03]);
}

#[test]
fn iso15765_extended_id_frame_is_flagged_for_device_padding_on_write() {
    let channel_id = connect(ISO15765, 0, 500_000);
    set_config(channel_id, &[(DATA_RATE, 500_000)]);

    let frame = iso15765::single_frame_extended(0x18DA10F1, &[0x3E, 0x00])
        .expect("extended single frame should build");
    let mut padded = iso15765::with_padding(frame);

    write_message(channel_id, &mut padded);

    let written = mock_get_written_messages(channel_id);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].protocol_id, ISO15765);
    assert_eq!(written[0].tx_flags, TX_EXTENDED_ID | TX_ISO15765_FRAME_PAD);
    assert_eq!(written[0].data, vec![0x18, 0xDA, 0x10, 0xF1, 0x3E, 0x00]);
}

#[test]
fn iso9141_protocol_sets_kline_timing_params_and_writes_request() {
    let channel_id = connect(ISO9141, 0, 10_400);

    let params = [
        (DATA_RATE, 10_400),
        (PARITY, 0),
        (P1_MIN, 0),
        (P1_MAX, 20),
        (P3_MIN, 55),
        (P3_MAX, 5000),
        (W1, 60),
        (W2, 20),
        (W3, 55),
        (W4, 5),
        (W5, 300),
        (TIDLE, 300),
        (TINIL, 25),
        (TWUP, 300),
    ];
    set_config(channel_id, &params);
    assert_config(channel_id, &params);

    let request = [0x68, 0x6A, 0xF1, 0x01, 0x00, 0xC5];
    let mut message = PassThruMessage::new(ISO9141, 0, 0, 0, 0, &request)
        .expect("ISO9141 request frame should build");

    write_message(channel_id, &mut message);

    let written = mock_get_written_messages(channel_id);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].protocol_id, ISO9141);
    assert_eq!(written[0].data, request);
}

#[test]
fn iso14230_protocol_sets_kwp_timing_params_and_writes_request() {
    let channel_id = connect(ISO14230, 0, 10_400);

    let params = [
        (DATA_RATE, 10_400),
        (PARITY, 0),
        (P1_MIN, 0),
        (P1_MAX, 20),
        (P2_MIN, 25),
        (P2_MAX, 50),
        (P3_MIN, 55),
        (P3_MAX, 5000),
    ];
    set_config(channel_id, &params);
    assert_config(channel_id, &params);

    // KWP2000 physical addressing request: format, target, source, data, checksum.
    let request = [0x81, 0x11, 0xF1, 0x22, 0x33, 0x37];
    let mut message = PassThruMessage::new(ISO14230, 0, 0, 0, 0, &request)
        .expect("ISO14230 request frame should build");

    write_message(channel_id, &mut message);

    let written = mock_get_written_messages(channel_id);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].protocol_id, ISO14230);
    assert_eq!(written[0].data, request);
}

#[test]
fn j1850vpw_protocol_sets_node_address_and_writes_frame() {
    let channel_id = connect(J1850VPW, 0, 10_400);

    let params = [(DATA_RATE, 10_400), (NODE_ADDRESS, 0xF1), (NETWORK_LINE, 0)];
    set_config(channel_id, &params);
    assert_config(channel_id, &params);

    let frame = [0x68, 0x6A, 0xF1, 0x01, 0x00];
    let mut message =
        PassThruMessage::new(J1850VPW, 0, 0, 0, 0, &frame).expect("J1850VPW frame should build");

    write_message(channel_id, &mut message);

    let written = mock_get_written_messages(channel_id);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].protocol_id, J1850VPW);
    assert_eq!(written[0].data, frame);
}

#[test]
fn j1850pwm_protocol_sets_node_address_and_writes_frame() {
    let channel_id = connect(J1850PWM, 0, 41_600);

    let params = [(DATA_RATE, 41_600), (NODE_ADDRESS, 0x10)];
    set_config(channel_id, &params);
    assert_config(channel_id, &params);

    let frame = [0x61, 0x6A, 0xF1, 0x01, 0x00];
    let mut message =
        PassThruMessage::new(J1850PWM, 0, 0, 0, 0, &frame).expect("J1850PWM frame should build");

    write_message(channel_id, &mut message);

    let written = mock_get_written_messages(channel_id);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].protocol_id, J1850PWM);
    assert_eq!(written[0].data, frame);
}
