//! The exports are called the way a worker calls them after loading the library. The simulated
//! bus is process-global, so the tests take a lock and start from a fresh ECU.

use super::*;
use sim_ecu::{Fault, Session};
use std::ptr;

static SERIAL: Mutex<()> = Mutex::new(());

const ISO15765: PassThruUlong = protocol::PROTOCOL_ISO15765 as PassThruUlong;

/// Holds the test lock, with the device open and one ISO 15765 channel connected.
struct Fixture {
    _serial: MutexGuard<'static, ()>,
    channel: PassThruUlong,
}

fn setup(config: EcuConfig) -> Fixture {
    let serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    reset(config);
    let mut device = 0;
    assert_eq!(
        unsafe { PassThruOpen(ptr::null(), &mut device) },
        STATUS_NOERROR
    );
    assert_eq!(device, DEVICE_ID);
    let mut channel = 0;
    assert_eq!(
        unsafe { PassThruConnect(device, ISO15765, 0, 500_000, &mut channel) },
        STATUS_NOERROR
    );
    assert_eq!(
        start_filter(channel, ECU_RESPONSE_ID, ECU_PHYSICAL_REQUEST_ID).0,
        STATUS_NOERROR
    );
    Fixture {
        _serial: serial,
        channel,
    }
}

fn fixture() -> Fixture {
    setup(default_ecu_config())
}

fn msg(can_id: u32, payload: &[u8]) -> PassThruMsg {
    let mut m = empty_msg();
    m.protocol_id = ISO15765;
    m.data[..4].copy_from_slice(&can_id.to_be_bytes());
    m.data[4..4 + payload.len()].copy_from_slice(payload);
    m.data_size = (4 + payload.len()) as PassThruUlong;
    m
}

/// Starts a flow-control filter receiving from `pattern` and sending to `flow_control`.
fn start_filter(
    channel: PassThruUlong,
    pattern: u32,
    flow_control: u32,
) -> (PassThruUlong, PassThruUlong) {
    let mask = msg_bytes(&[0xFF; 4]);
    let pattern = msg_bytes(&pattern.to_be_bytes());
    let flow_control = msg_bytes(&flow_control.to_be_bytes());
    let mut id = 0;
    let status = unsafe {
        PassThruStartMsgFilter(
            channel,
            filter::FLOW_CONTROL_FILTER as PassThruUlong,
            &mask,
            &pattern,
            &flow_control,
            &mut id,
        )
    };
    (status, id)
}

/// An ISO 15765 message holding exactly `data`.
fn msg_bytes(data: &[u8]) -> PassThruMsg {
    let mut m = empty_msg();
    m.protocol_id = ISO15765;
    m.data[..data.len()].copy_from_slice(data);
    m.data_size = data.len() as PassThruUlong;
    m
}

fn empty_msg() -> PassThruMsg {
    PassThruMsg {
        protocol_id: 0,
        rx_status: 0,
        tx_flags: 0,
        timestamp: 0,
        data_size: 0,
        extra_data_index: 0,
        data: [0; MAX_MSG_DATA],
    }
}

fn write(channel: PassThruUlong, can_id: u32, payload: &[u8]) {
    let m = msg(can_id, payload);
    let mut n = 1;
    assert_eq!(
        unsafe { PassThruWriteMsgs(channel, &m, &mut n, 100) },
        STATUS_NOERROR
    );
    assert_eq!(n, 1);
}

/// Reads up to `wanted` messages; returns the status and the data of each message read.
fn read(channel: PassThruUlong, wanted: usize, timeout: u32) -> (PassThruUlong, Vec<Vec<u8>>) {
    let mut buf: Vec<PassThruMsg> = (0..wanted).map(|_| empty_msg()).collect();
    let mut n = wanted as PassThruUlong;
    let status = unsafe {
        PassThruReadMsgs(
            channel,
            buf.as_mut_ptr(),
            &mut n,
            PassThruUlong::from(timeout),
        )
    };
    let msgs = buf[..n as usize]
        .iter()
        .map(|m| {
            assert_eq!(m.protocol_id, ISO15765);
            m.data[..m.data_size as usize].to_vec()
        })
        .collect();
    (status, msgs)
}

fn response(payload: &[u8]) -> Vec<u8> {
    let mut v = ECU_RESPONSE_ID.to_be_bytes().to_vec();
    v.extend_from_slice(payload);
    v
}

#[test]
fn a_written_request_is_answered_by_sim_ecu() {
    let f = fixture();
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x90]);
    let mut expected = vec![0x62, 0xF1, 0x90];
    expected.extend_from_slice(b"NGRSIMECU00000001");
    assert_eq!(
        read(f.channel, 1, 100),
        (STATUS_NOERROR, vec![response(&expected)])
    );
}

#[test]
fn responses_are_read_in_order_and_negative_responses_pass_through() {
    let f = fixture();
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x10, 0x03]);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x99]);
    let (status, msgs) = read(f.channel, 3, 0);
    assert_eq!(status, STATUS_NOERROR);
    assert_eq!(
        msgs,
        [
            response(&[0x50, 0x03, 0x00, 0x32, 0x01, 0xF4]),
            response(&[0x62, 0xF1, 0x86, 0x03]),
            response(&[0x7F, 0x99, 0x11]),
        ]
    );
}

#[test]
fn functional_requests_reach_the_ecu_and_other_ids_do_not() {
    let f = fixture();
    write(f.channel, FUNCTIONAL_REQUEST_ID, &[0x3E, 0x00]);
    // NRC 11 is not sent for a functionally addressed request.
    write(f.channel, FUNCTIONAL_REQUEST_ID, &[0x99]);
    write(f.channel, 0x7E1, &[0x3E, 0x00]);
    assert_eq!(
        read(f.channel, 3, 0),
        (STATUS_NOERROR, vec![response(&[0x7E, 0x00])])
    );
}

#[test]
fn an_empty_buffer_reports_buffer_empty_and_a_short_read_times_out() {
    let f = fixture();
    assert_eq!(read(f.channel, 1, 0), (ERR_BUFFER_EMPTY, vec![]));
    assert_eq!(read(f.channel, 1, 20), (ERR_BUFFER_EMPTY, vec![]));
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(
        read(f.channel, 2, 20),
        (ERR_TIMEOUT, vec![response(&[0x7E, 0x00])])
    );
}

#[test]
fn the_configured_response_delay_is_applied() {
    let f = setup(EcuConfig {
        response_delay_ms: 50,
        ..default_ecu_config()
    });
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(read(f.channel, 1, 0), (ERR_BUFFER_EMPTY, vec![]));
    let start = Instant::now();
    assert_eq!(
        read(f.channel, 1, 1000),
        (STATUS_NOERROR, vec![response(&[0x7E, 0x00])])
    );
    assert!(start.elapsed() < Duration::from_millis(1000));
}

#[test]
fn an_injected_delay_holds_back_only_that_response() {
    let f = fixture();
    with_ecu(|ecu| ecu.inject(Fault::DelayResponse { ms: 200 }));
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    // The undelayed response appears on the bus first.
    assert_eq!(
        read(f.channel, 1, 100),
        (STATUS_NOERROR, vec![response(&[0x62, 0xF1, 0x86, 0x01])])
    );
    assert_eq!(read(f.channel, 1, 0), (ERR_BUFFER_EMPTY, vec![]));
    assert_eq!(
        read(f.channel, 1, 1000),
        (STATUS_NOERROR, vec![response(&[0x7E, 0x00])])
    );
}

#[test]
fn a_dropped_response_times_out_but_the_request_took_effect() {
    let f = fixture();
    with_ecu(|ecu| ecu.inject(Fault::DropResponse));
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x10, 0x02]);
    assert_eq!(read(f.channel, 1, 20), (ERR_BUFFER_EMPTY, vec![]));
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(
        read(f.channel, 1, 100),
        (STATUS_NOERROR, vec![response(&[0x62, 0xF1, 0x86, 0x02])])
    );
}

#[test]
fn a_bus_error_loses_the_request() {
    let f = fixture();
    with_ecu(|ecu| ecu.inject(Fault::BusError));
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x10, 0x03]);
    assert_eq!(read(f.channel, 1, 20), (ERR_BUFFER_EMPTY, vec![]));
    assert_eq!(with_ecu(|ecu| ecu.session), Session::Default);
}

#[test]
fn a_power_loss_silences_the_ecu_until_reconnect() {
    let f = fixture();
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x10, 0x03]);
    assert_eq!(read(f.channel, 1, 100).0, STATUS_NOERROR);
    with_ecu(|ecu| ecu.inject(Fault::PowerLoss));
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(read(f.channel, 1, 20), (ERR_BUFFER_EMPTY, vec![]));
    with_ecu(|ecu| ecu.reconnect());
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(
        read(f.channel, 1, 100),
        (STATUS_NOERROR, vec![response(&[0x62, 0xF1, 0x86, 0x01])])
    );
}

#[test]
fn a_corrupt_block_is_answered_with_nrc_72() {
    let f = fixture();
    for request in [
        &[0x10, 0x02][..],
        &[0x31, 0x01, 0xFF, 0x00],
        &[0x34, 0x00, 0x44, 0, 0, 0, 0, 0, 0, 0, 2],
    ] {
        write(f.channel, ECU_PHYSICAL_REQUEST_ID, request);
        let (status, msgs) = read(f.channel, 1, 100);
        assert_eq!(status, STATUS_NOERROR);
        assert_ne!(msgs[0][4], 0x7F, "{request:02X?} -> {msgs:02X?}");
    }
    with_ecu(|ecu| ecu.inject(Fault::CorruptBlock { block: 1 }));
    write(
        f.channel,
        ECU_PHYSICAL_REQUEST_ID,
        &[0x36, 0x01, 0xAA, 0xBB],
    );
    write(
        f.channel,
        ECU_PHYSICAL_REQUEST_ID,
        &[0x36, 0x01, 0xAA, 0xBB],
    );
    assert_eq!(
        read(f.channel, 2, 100),
        (
            STATUS_NOERROR,
            vec![response(&[0x7F, 0x36, 0x72]), response(&[0x76, 0x01])]
        )
    );
}

#[test]
fn the_ecu_keeps_its_state_across_close_and_open() {
    let f = fixture();
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x10, 0x03]);
    assert_eq!(PassThruClose(DEVICE_ID), STATUS_NOERROR);
    let mut device = 0;
    assert_eq!(
        unsafe { PassThruOpen(ptr::null(), &mut device) },
        STATUS_NOERROR
    );
    let mut channel = 0;
    assert_eq!(
        unsafe { PassThruConnect(device, ISO15765, 0, 500_000, &mut channel) },
        STATUS_NOERROR
    );
    assert_eq!(
        start_filter(channel, ECU_RESPONSE_ID, ECU_PHYSICAL_REQUEST_ID).0,
        STATUS_NOERROR
    );
    // The old channel and its unread response are gone.
    assert_eq!(read(f.channel, 1, 0).0, ERR_INVALID_CHANNEL_ID);
    write(channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(
        read(channel, 1, 100),
        (STATUS_NOERROR, vec![response(&[0x62, 0xF1, 0x86, 0x03])])
    );
}

#[test]
fn invalid_handles_and_messages_are_rejected() {
    let f = fixture();
    let mut channel = 0;
    assert_eq!(
        unsafe { PassThruConnect(DEVICE_ID + 1, ISO15765, 0, 500_000, &mut channel) },
        ERR_INVALID_DEVICE_ID
    );
    assert_eq!(
        unsafe {
            PassThruConnect(
                DEVICE_ID,
                protocol::PROTOCOL_CAN as PassThruUlong,
                0,
                500_000,
                &mut channel,
            )
        },
        ERR_NOT_SUPPORTED
    );
    assert_eq!(
        unsafe { PassThruConnect(DEVICE_ID, ISO15765, 0, 500_000, ptr::null_mut()) },
        ERR_NULL_PARAMETER
    );

    let mut n = 1;
    let m = msg(ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(
        unsafe { PassThruWriteMsgs(f.channel + 100, &m, &mut n, 0) },
        ERR_INVALID_CHANNEL_ID
    );
    assert_eq!(
        unsafe { PassThruWriteMsgs(f.channel, ptr::null(), &mut n, 0) },
        ERR_NULL_PARAMETER
    );

    // The second message is rejected; the first was sent.
    let mut bad_protocol = msg(ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    bad_protocol.protocol_id = protocol::PROTOCOL_CAN as PassThruUlong;
    let pair = [msg(ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]), bad_protocol];
    let mut n = 2;
    assert_eq!(
        unsafe { PassThruWriteMsgs(f.channel, pair.as_ptr(), &mut n, 0) },
        ERR_MSG_PROTOCOL_ID
    );
    assert_eq!(n, 1);

    let mut no_payload = msg(ECU_PHYSICAL_REQUEST_ID, &[]);
    no_payload.data_size = 4;
    let mut n = 1;
    assert_eq!(
        unsafe { PassThruWriteMsgs(f.channel, &no_payload, &mut n, 0) },
        ERR_INVALID_MSG
    );
    assert_eq!(n, 0);

    assert_eq!(PassThruDisconnect(f.channel), STATUS_NOERROR);
    assert_eq!(PassThruDisconnect(f.channel), ERR_INVALID_CHANNEL_ID);
    assert_eq!(PassThruClose(DEVICE_ID), STATUS_NOERROR);
    assert_eq!(PassThruClose(DEVICE_ID), ERR_INVALID_DEVICE_ID);
}

#[test]
fn a_read_waiting_for_a_response_wakes_when_another_thread_writes() {
    let f = fixture();
    let channel = f.channel;
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        write(channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    });
    assert_eq!(
        read(channel, 1, 2000),
        (STATUS_NOERROR, vec![response(&[0x7E, 0x00])])
    );
    writer.join().unwrap();
}

#[test]
fn a_maximal_timeout_does_not_overflow() {
    let f = fixture();
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    let mut m = empty_msg();
    let mut n = 1;
    assert_eq!(
        unsafe { PassThruReadMsgs(f.channel, &mut m, &mut n, PassThruUlong::MAX) },
        STATUS_NOERROR
    );
    assert_eq!(n, 1);
}

#[test]
fn a_power_loss_discards_responses_not_yet_sent() {
    let f = fixture();
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    with_ecu(|ecu| ecu.inject(Fault::DelayResponse { ms: 100 }));
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    with_ecu(|ecu| ecu.inject(Fault::PowerLoss));
    // The undelayed response was already on the bus; the delayed one was never sent.
    assert_eq!(
        read(f.channel, 2, 300),
        (ERR_TIMEOUT, vec![response(&[0x7E, 0x00])])
    );
}

#[test]
fn an_ecu_reset_response_survives_its_own_reset() {
    let f = setup(EcuConfig {
        response_delay_ms: 200,
        ..default_ecu_config()
    });
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x11, 0x01]);
    // The reset discards the TesterPresent response still being delayed, not its own.
    assert_eq!(
        read(f.channel, 2, 600),
        (ERR_TIMEOUT, vec![response(&[0x51, 0x01])])
    );
}

#[test]
fn timestamps_mark_when_each_response_appeared() {
    let f = fixture();
    with_ecu(|ecu| ecu.inject(Fault::DelayResponse { ms: 200 }));
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    std::thread::sleep(Duration::from_millis(300));
    let mut buf = [empty_msg(), empty_msg()];
    let mut n = 2;
    assert_eq!(
        unsafe { PassThruReadMsgs(f.channel, buf.as_mut_ptr(), &mut n, 0) },
        STATUS_NOERROR
    );
    // Read together, but the delayed response appeared about 200 ms after the other.
    let gap = buf[1].timestamp.wrapping_sub(buf[0].timestamp);
    assert!((100_000..=200_000).contains(&gap), "gap {gap} us");
}

/// J2534-1 7.2.9: an ISO 15765 channel receives only what a flow-control filter lets in, and a
/// segmented request needs a filter whose flow-control ID is the request's.
#[test]
fn flow_control_filters_gate_reception_and_segmented_sends() {
    let f = fixture();
    let mut channel = 0;
    assert_eq!(
        unsafe { PassThruConnect(DEVICE_ID, ISO15765, 0, 500_000, &mut channel) },
        STATUS_NOERROR
    );
    // Without a filter the ECU answers on the bus, but nothing is received.
    write(channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(read(channel, 1, 50).0, ERR_BUFFER_EMPTY);
    // A segmented request is refused without a matching flow-control ID, sent with one.
    let long = msg(ECU_PHYSICAL_REQUEST_ID, &[0x2E, 0xF1, 0x90, 1, 2, 3, 4, 5]);
    let mut n = 1;
    assert_eq!(
        unsafe { PassThruWriteMsgs(channel, &long, &mut n, 100) },
        ERR_NO_FLOW_CONTROL
    );
    assert_eq!(n, 0);
    let (status, id) = start_filter(channel, ECU_RESPONSE_ID, ECU_PHYSICAL_REQUEST_ID);
    assert_eq!(status, STATUS_NOERROR);
    let mut n = 1;
    assert_eq!(
        unsafe { PassThruWriteMsgs(channel, &long, &mut n, 100) },
        STATUS_NOERROR
    );
    assert_eq!(n, 1);
    assert_eq!(read(channel, 1, 100).0, STATUS_NOERROR);
    // Stopping the filter stops reception again.
    assert_eq!(PassThruStopMsgFilter(channel, id), STATUS_NOERROR);
    assert_eq!(PassThruStopMsgFilter(channel, id), ERR_INVALID_FILTER_ID);
    write(channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(read(channel, 1, 50).0, ERR_BUFFER_EMPTY);
    // The fixture's own channel is unaffected.
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(read(f.channel, 1, 100).0, STATUS_NOERROR);
}

#[test]
fn flow_control_filters_are_validated() {
    let f = fixture();
    let ch = f.channel;
    let call = |kind: u32, mask: &[u8], pattern: &[u8], flow: &[u8]| {
        let (mask, pattern, flow) = (msg_bytes(mask), msg_bytes(pattern), msg_bytes(flow));
        let mut id = 0;
        unsafe {
            PassThruStartMsgFilter(ch, kind as PassThruUlong, &mask, &pattern, &flow, &mut id)
        }
    };
    let ff = [0xFF; 4];
    let id = |v: u32| v.to_be_bytes();
    let fc = filter::FLOW_CONTROL_FILTER;
    // Only flow-control filters on ISO 15765.
    assert_eq!(
        call(filter::PASS_FILTER, &ff, &id(0x700), &id(0x701)),
        ERR_INVALID_FILTER_ID
    );
    // The mask must select the whole 4-byte CAN ID, and all three messages must match in size.
    assert_eq!(
        call(fc, &[0xFF, 0xFF, 0xFF, 0x00], &id(0x700), &id(0x701)),
        ERR_INVALID_MSG
    );
    assert_eq!(
        call(fc, &[0xFF; 5], &[0, 0, 7, 0, 1], &[0, 0, 7, 1, 1]),
        ERR_INVALID_MSG
    );
    assert_eq!(call(fc, &ff, &[0, 0, 7, 0, 1], &id(0x701)), ERR_INVALID_MSG);
    // IDs already used by the fixture's filter (7E8 / 7E0) are not unique.
    assert_eq!(
        call(fc, &ff, &id(ECU_RESPONSE_ID), &id(0x701)),
        ERR_NOT_UNIQUE
    );
    assert_eq!(
        call(fc, &ff, &id(0x700), &id(ECU_PHYSICAL_REQUEST_ID)),
        ERR_NOT_UNIQUE
    );
    // Pattern and flow-control IDs may be the same within one filter (functional reception).
    assert_eq!(call(fc, &ff, &id(0x7DF), &id(0x7DF)), STATUS_NOERROR);
    // Ten filters per channel; the fixture and the one above use two.
    for n in 0..8 {
        assert_eq!(
            call(fc, &ff, &id(0x600 + n), &id(0x680 + n)),
            STATUS_NOERROR
        );
    }
    assert_eq!(call(fc, &ff, &id(0x610), &id(0x690)), ERR_EXCEEDED_LIMIT);
    // Null pointers and unknown channels.
    let mut id_out = 0;
    let m = msg_bytes(&ff);
    assert_eq!(
        unsafe {
            PassThruStartMsgFilter(ch, fc as PassThruUlong, &m, &m, ptr::null(), &mut id_out)
        },
        ERR_NULL_PARAMETER
    );
    assert_eq!(
        unsafe { PassThruStartMsgFilter(99, fc as PassThruUlong, &m, &m, &m, &mut id_out) },
        ERR_INVALID_CHANNEL_ID
    );
    assert_eq!(PassThruStopMsgFilter(99, 1), ERR_INVALID_CHANNEL_ID);
}

#[test]
fn version_and_other_exports() {
    let f = fixture();
    let mut fw = [1 as c_char; 80];
    let mut dll = [1 as c_char; 80];
    let mut api = [1 as c_char; 80];
    assert_eq!(
        unsafe {
            PassThruReadVersion(
                DEVICE_ID,
                fw.as_mut_ptr(),
                dll.as_mut_ptr(),
                api.as_mut_ptr(),
            )
        },
        STATUS_NOERROR
    );
    let text = |b: &[c_char; 80]| {
        unsafe { std::ffi::CStr::from_ptr(b.as_ptr()) }
            .to_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(text(&fw), FIRMWARE_VERSION);
    assert_eq!(text(&dll), DLL_VERSION);
    assert_eq!(text(&api), API_VERSION);
    assert_eq!(
        unsafe { PassThruReadVersion(7, fw.as_mut_ptr(), dll.as_mut_ptr(), api.as_mut_ptr()) },
        ERR_INVALID_DEVICE_ID
    );
    assert_eq!(
        unsafe {
            PassThruReadVersion(
                DEVICE_ID,
                ptr::null_mut(),
                dll.as_mut_ptr(),
                api.as_mut_ptr(),
            )
        },
        ERR_NULL_PARAMETER
    );

    let mut desc = [1 as c_char; 80];
    assert_eq!(
        unsafe { PassThruGetLastError(desc.as_mut_ptr()) },
        STATUS_NOERROR
    );
    assert_eq!(text(&desc), LAST_ERROR_TEXT);
    assert_eq!(
        unsafe { PassThruGetLastError(ptr::null_mut()) },
        ERR_NULL_PARAMETER
    );

    let m = msg(ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x80]);
    let mut msg_id = 0;
    assert_eq!(
        PassThruStartPeriodicMsg(f.channel, &m, &mut msg_id, 2000),
        ERR_NOT_SUPPORTED
    );
    assert_eq!(PassThruStopPeriodicMsg(f.channel, 1), ERR_INVALID_MSG_ID);
    assert_eq!(
        PassThruStartPeriodicMsg(99, &m, &mut msg_id, 2000),
        ERR_INVALID_CHANNEL_ID
    );
    assert_eq!(PassThruStopPeriodicMsg(99, 1), ERR_INVALID_CHANNEL_ID);
}

/// J2534-1 7.2.11: one programming pin at a time, valid pins and voltages, pin 15 ground only.
#[test]
fn programming_voltage_follows_the_pin_rules() {
    let _f = fixture();
    let set = |pin, voltage| PassThruSetProgrammingVoltage(DEVICE_ID, pin, voltage);
    assert_eq!(set(12, 18000), STATUS_NOERROR);
    // Another pin while 12 is on, until 12 is switched off.
    assert_eq!(set(13, 18000), ERR_PIN_INVALID);
    assert_eq!(set(12, VOLTAGE_OFF), STATUS_NOERROR);
    assert_eq!(set(13, 5000), STATUS_NOERROR);
    assert_eq!(set(13, 20000), STATUS_NOERROR);
    // Pin 15 is shorted to ground alongside, never driven.
    assert_eq!(set(15, SHORT_TO_GROUND), STATUS_NOERROR);
    assert_eq!(set(15, 12000), ERR_PIN_INVALID);
    assert_eq!(set(13, SHORT_TO_GROUND), ERR_PIN_INVALID);
    // Pins and voltages outside the lists.
    assert_eq!(set(7, 12000), ERR_PIN_INVALID);
    assert_eq!(set(13, 4999), ERR_FAILED);
    assert_eq!(set(13, 20001), ERR_FAILED);
    assert_eq!(
        PassThruSetProgrammingVoltage(7, 13, 12000),
        ERR_INVALID_DEVICE_ID
    );
    // Closing the device switches everything off.
    assert_eq!(PassThruClose(DEVICE_ID), STATUS_NOERROR);
    let mut device = 0;
    assert_eq!(
        unsafe { PassThruOpen(ptr::null(), &mut device) },
        STATUS_NOERROR
    );
    assert_eq!(set(12, 12000), STATUS_NOERROR);
}

/// `CLEAR_MSG_FILTERS` removes the channel's filters, so the same IDs can be installed again, and
/// `CLEAR_RX_BUFFER` drops responses already received.
#[test]
fn ioctl_clears_filters_and_the_receive_buffer() {
    let f = fixture();
    let clear = |id: u32| unsafe {
        PassThruIoctl(
            f.channel,
            id as PassThruUlong,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    std::thread::sleep(Duration::from_millis(20));
    assert_eq!(clear(ioctl::IOCTL_CLEAR_RX_BUFFER), STATUS_NOERROR);
    assert_eq!(read(f.channel, 1, 0).0, ERR_BUFFER_EMPTY);

    assert_eq!(clear(ioctl::IOCTL_CLEAR_MSG_FILTERS), STATUS_NOERROR);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(read(f.channel, 1, 50).0, ERR_BUFFER_EMPTY);
    assert_eq!(
        start_filter(f.channel, ECU_RESPONSE_ID, ECU_PHYSICAL_REQUEST_ID).0,
        STATUS_NOERROR
    );
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(read(f.channel, 1, 100).0, STATUS_NOERROR);

    assert_eq!(
        unsafe {
            PassThruIoctl(
                99,
                ioctl::IOCTL_CLEAR_MSG_FILTERS as PassThruUlong,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        },
        ERR_INVALID_CHANNEL_ID
    );
}

/// A segmented response is received only through a filter whose flow-control ID is the ECU's
/// request ID, where flow control for it goes; a pattern-equals-flow-control filter receives
/// single frames only.
#[test]
fn segmented_responses_need_flow_control_to_the_ecu() {
    let _f = fixture();
    let vin_request = [0x22, 0xF1, 0x90]; // 20-byte response
    let short_request = [0x22, 0xF1, 0x86]; // 4-byte response
    for (flow_control, receives_vin) in [
        (ECU_RESPONSE_ID, false),
        (0x7E1, false),
        (ECU_PHYSICAL_REQUEST_ID, true),
    ] {
        let mut channel = 0;
        assert_eq!(
            unsafe { PassThruConnect(DEVICE_ID, ISO15765, 0, 500_000, &mut channel) },
            STATUS_NOERROR
        );
        assert_eq!(
            start_filter(channel, ECU_RESPONSE_ID, flow_control).0,
            STATUS_NOERROR
        );
        write(channel, ECU_PHYSICAL_REQUEST_ID, &short_request);
        assert_eq!(read(channel, 1, 100).0, STATUS_NOERROR, "{flow_control:#x}");
        write(channel, ECU_PHYSICAL_REQUEST_ID, &vin_request);
        let expected = if receives_vin {
            STATUS_NOERROR
        } else {
            ERR_BUFFER_EMPTY
        };
        assert_eq!(read(channel, 1, 50).0, expected, "{flow_control:#x}");
        assert_eq!(PassThruDisconnect(channel), STATUS_NOERROR);
    }
}

/// Filters judge a response when it appears on the bus, not when the request is written.
#[test]
fn filters_apply_when_the_response_appears() {
    let _f = setup(EcuConfig {
        response_delay_ms: 100,
        ..default_ecu_config()
    });
    let mut channel = 0;
    assert_eq!(
        unsafe { PassThruConnect(DEVICE_ID, ISO15765, 0, 500_000, &mut channel) },
        STATUS_NOERROR
    );
    // A filter started while the response is delayed lets it in.
    write(channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    let (status, id) = start_filter(channel, ECU_RESPONSE_ID, ECU_PHYSICAL_REQUEST_ID);
    assert_eq!(status, STATUS_NOERROR);
    assert_eq!(read(channel, 1, 500).0, STATUS_NOERROR);
    // A filter stopped while the response is delayed keeps it out.
    write(channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(PassThruStopMsgFilter(channel, id), STATUS_NOERROR);
    assert_eq!(read(channel, 1, 300).0, ERR_BUFFER_EMPTY);
    // A response that appeared while a filter was in place stays readable after it stops.
    let (status, id) = start_filter(channel, ECU_RESPONSE_ID, ECU_PHYSICAL_REQUEST_ID);
    assert_eq!(status, STATUS_NOERROR);
    write(channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(PassThruStopMsgFilter(channel, id), STATUS_NOERROR);
    assert_eq!(read(channel, 1, 0).0, STATUS_NOERROR);
}

#[test]
fn filter_and_write_validation_edges() {
    let f = fixture();
    let ch = f.channel;
    let fc = filter::FLOW_CONTROL_FILTER as PassThruUlong;
    let start = |mask: PassThruMsg, pattern: PassThruMsg, flow: PassThruMsg| {
        let mut id = 0;
        unsafe { PassThruStartMsgFilter(ch, fc, &mask, &pattern, &flow, &mut id) }
    };
    let ff = || msg_bytes(&[0xFF; 4]);
    let id = |v: u32| msg_bytes(&v.to_be_bytes());
    // An ID may not reappear in another filter in the other role either (fixture: 7E8 / 7E0).
    assert_eq!(
        start(ff(), id(ECU_PHYSICAL_REQUEST_ID), id(0x701)),
        ERR_NOT_UNIQUE
    );
    assert_eq!(start(ff(), id(0x700), id(ECU_RESPONSE_ID)), ERR_NOT_UNIQUE);
    // TxFlags must match across the three messages.
    let mut flagged = id(0x701);
    flagged.tx_flags = 0x40;
    assert_eq!(start(ff(), id(0x700), flagged), ERR_INVALID_MSG);
    // Protocol must be the channel's.
    let mut other = id(0x700);
    other.protocol_id = protocol::PROTOCOL_CAN as PassThruUlong;
    assert_eq!(start(ff(), other, id(0x701)), ERR_MSG_PROTOCOL_ID);
    // 29-bit IDs, extended addressing and IDs beyond 11 bits are not used on this channel.
    for flag in [
        tx_flag::TX_FLAG_CAN_29BIT_ID,
        tx_flag::TX_FLAG_ISO15765_ADDR_TYPE,
    ] {
        let with = |mut m: PassThruMsg| {
            m.tx_flags = flag as PassThruUlong;
            m
        };
        assert_eq!(
            start(with(ff()), with(id(0x700)), with(id(0x701))),
            ERR_INVALID_MSG
        );
        let mut request = msg(ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
        request.tx_flags = flag as PassThruUlong;
        let mut n = 1;
        assert_eq!(
            unsafe { PassThruWriteMsgs(ch, &request, &mut n, 100) },
            ERR_INVALID_MSG
        );
        assert_eq!(n, 0);
    }
    assert_eq!(start(ff(), id(0x800), id(0x701)), ERR_INVALID_MSG);
    assert_eq!(start(ff(), id(0x700), id(0x800)), ERR_INVALID_MSG);

    // A 7-byte payload is a single frame and needs no filter; 8 bytes do.
    let mut bare = 0;
    assert_eq!(
        unsafe { PassThruConnect(DEVICE_ID, ISO15765, 0, 500_000, &mut bare) },
        STATUS_NOERROR
    );
    for (len, expected) in [(7, STATUS_NOERROR), (8, ERR_NO_FLOW_CONTROL)] {
        let m = msg(ECU_PHYSICAL_REQUEST_ID, &vec![0x31; len]);
        let mut n = 1;
        assert_eq!(
            unsafe { PassThruWriteMsgs(bare, &m, &mut n, 100) },
            expected,
            "{len}"
        );
    }
}

/// A segmented request needs a filter that sends to it and receives from the partner answering
/// there; a pattern-equals-flow-control filter (functional) never authorizes one.
#[test]
fn segmented_writes_need_the_partner_filter() {
    let _f = fixture();
    let long = |can_id| msg(can_id, &[0x2E, 0xF1, 0x90, 1, 2, 3, 4, 5]);
    for (pattern, flow_control, target, expected) in [
        // Flow control to the ECU, but the pattern is not the ECU's response ID.
        (
            0x700,
            ECU_PHYSICAL_REQUEST_ID,
            ECU_PHYSICAL_REQUEST_ID,
            ERR_NO_FLOW_CONTROL,
        ),
        // A functional filter: single frames only.
        (
            FUNCTIONAL_REQUEST_ID,
            FUNCTIONAL_REQUEST_ID,
            FUNCTIONAL_REQUEST_ID,
            ERR_NO_FLOW_CONTROL,
        ),
        // Functional requests are never segmented, whatever the filter.
        (
            0x700,
            FUNCTIONAL_REQUEST_ID,
            FUNCTIONAL_REQUEST_ID,
            ERR_NO_FLOW_CONTROL,
        ),
        // The partner filter.
        (
            ECU_RESPONSE_ID,
            ECU_PHYSICAL_REQUEST_ID,
            ECU_PHYSICAL_REQUEST_ID,
            STATUS_NOERROR,
        ),
        // No simulated responder on 0x710: any distinct pattern will do.
        (0x718, 0x710, 0x710, STATUS_NOERROR),
    ] {
        let mut channel = 0;
        assert_eq!(
            unsafe { PassThruConnect(DEVICE_ID, ISO15765, 0, 500_000, &mut channel) },
            STATUS_NOERROR
        );
        assert_eq!(
            start_filter(channel, pattern, flow_control).0,
            STATUS_NOERROR
        );
        let m = long(target);
        let mut n = 1;
        assert_eq!(
            unsafe { PassThruWriteMsgs(channel, &m, &mut n, 100) },
            expected,
            "{pattern:#x}/{flow_control:#x} -> {target:#x}"
        );
        assert_eq!(PassThruDisconnect(channel), STATUS_NOERROR);
    }
}

// ---------------------------------------------------------------- Control

/// Applies `command` through the control export.
fn control(command: &str) -> PassThruUlong {
    let command = std::ffi::CString::new(command).expect("no NUL in the command");
    unsafe { NgrSimVciControl(command.as_ptr()) }
}

#[test]
fn the_control_export_arms_faults_and_reconnects_the_ecu() {
    let f = fixture();
    assert_eq!(
        control(r#"{"command": "inject_fault", "fault": {"delay_response": {"ms": 50}}}"#),
        STATUS_NOERROR
    );
    assert_eq!(
        with_ecu(|ecu| ecu.armed_faults().to_vec()),
        [Fault::DelayResponse { ms: 50 }]
    );
    assert_eq!(
        control(r#"{"command": "inject_fault", "fault": "power_loss"}"#),
        STATUS_NOERROR
    );
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(read(f.channel, 1, 20), (ERR_BUFFER_EMPTY, vec![]));
    assert_eq!(control(r#"{"command": "reconnect_ecu"}"#), STATUS_NOERROR);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(
        read(f.channel, 1, 1000),
        (STATUS_NOERROR, vec![response(&[0x7E, 0x00])])
    );
}

#[test]
fn the_control_export_rejects_what_it_cannot_parse() {
    let _f = fixture();
    assert_eq!(unsafe { NgrSimVciControl(ptr::null()) }, ERR_NULL_PARAMETER);
    for command in [
        "",
        "not json",
        r#"{"command": "explode"}"#,
        r#"{"command": "inject_fault", "fault": "melt"}"#,
        r#"{"command": "reconnect_ecu", "extra": 1}"#,
    ] {
        assert_eq!(control(command), ERR_FAILED, "{command}");
    }
}

#[test]
fn a_lost_device_stays_lost_until_closed_and_reopens_with_a_new_id() {
    let f = fixture();
    assert_eq!(control(r#"{"command": "disconnect_vci"}"#), STATUS_NOERROR);
    let mut version = [[0 as c_char; 80]; 3];
    let [fw, dll, api] = &mut version;
    assert_eq!(
        unsafe {
            PassThruReadVersion(
                DEVICE_ID,
                fw.as_mut_ptr(),
                dll.as_mut_ptr(),
                api.as_mut_ptr(),
            )
        },
        ERR_DEVICE_NOT_CONNECTED
    );
    let m = msg(ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    let mut n = 1;
    assert_eq!(
        unsafe { PassThruWriteMsgs(f.channel, &m, &mut n, 100) },
        ERR_DEVICE_NOT_CONNECTED
    );
    assert_eq!(n, 0);
    assert_eq!(read(f.channel, 1, 0), (ERR_DEVICE_NOT_CONNECTED, vec![]));
    assert_eq!(
        unsafe { PassThruIoctl(f.channel, 0, ptr::null_mut(), ptr::null_mut()) },
        ERR_DEVICE_NOT_CONNECTED
    );
    let mut device = 0;
    assert_eq!(
        unsafe { PassThruOpen(ptr::null(), &mut device) },
        ERR_DEVICE_NOT_CONNECTED
    );
    // Plugged back in, the device stays lost until it is closed (J2534-1 6.10.1).
    assert_eq!(control(r#"{"command": "connect_vci"}"#), STATUS_NOERROR);
    assert_eq!(read(f.channel, 1, 0), (ERR_DEVICE_NOT_CONNECTED, vec![]));
    assert_eq!(PassThruClose(DEVICE_ID + 1), ERR_DEVICE_NOT_CONNECTED);
    assert_eq!(PassThruClose(DEVICE_ID), ERR_DEVICE_NOT_CONNECTED);
    assert_eq!(read(f.channel, 1, 0), (ERR_INVALID_CHANNEL_ID, vec![]));
    assert_eq!(
        unsafe { PassThruOpen(ptr::null(), &mut device) },
        STATUS_NOERROR
    );
    assert_eq!(device, DEVICE_ID + 1);
    let mut channel = 0;
    assert_eq!(
        unsafe { PassThruConnect(DEVICE_ID, ISO15765, 0, 500_000, &mut channel) },
        ERR_INVALID_DEVICE_ID
    );
    assert_eq!(
        unsafe { PassThruConnect(device, ISO15765, 0, 500_000, &mut channel) },
        STATUS_NOERROR
    );
    assert_eq!(PassThruClose(device), STATUS_NOERROR);
}

#[test]
fn a_vci_unplugged_while_closed_only_fails_the_open_until_plugged_back() {
    let f = fixture();
    assert_eq!(PassThruClose(DEVICE_ID), STATUS_NOERROR);
    assert_eq!(control(r#"{"command": "disconnect_vci"}"#), STATUS_NOERROR);
    let mut device = 0;
    assert_eq!(
        unsafe { PassThruOpen(ptr::null(), &mut device) },
        ERR_DEVICE_NOT_CONNECTED
    );
    // With no device open, calls that take a device or channel ID fail as they do for any
    // closed device.
    assert_eq!(PassThruClose(DEVICE_ID), ERR_INVALID_DEVICE_ID);
    assert_eq!(PassThruDisconnect(f.channel), ERR_INVALID_CHANNEL_ID);
    assert_eq!(control(r#"{"command": "connect_vci"}"#), STATUS_NOERROR);
    assert_eq!(
        unsafe { PassThruOpen(ptr::null(), &mut device) },
        STATUS_NOERROR
    );
    assert_eq!(device, DEVICE_ID);
}

#[test]
fn a_disconnect_ends_a_waiting_read() {
    let f = fixture();
    let start = Instant::now();
    let unplug = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(50));
        control(r#"{"command": "disconnect_vci"}"#)
    });
    let result = read(f.channel, 1, 5000);
    let elapsed = start.elapsed();
    // Joined before asserting, so a failure cannot unplug the VCI under the next test.
    assert_eq!(unplug.join().expect("no panic"), STATUS_NOERROR);
    assert_eq!(result, (ERR_DEVICE_NOT_CONNECTED, vec![]));
    assert!(elapsed < Duration::from_millis(4000));
}

/// A fresh, empty control directory, removed when the test ends.
struct ControlDir(PathBuf);

impl ControlDir {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("sim-vci-control-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("control directory");
        lock().control_dir = Some(dir.clone());
        Self(dir)
    }

    /// Writes a command file the way a test should: under a temporary name, then renamed.
    fn send(&self, name: &str, command: &str) {
        let tmp = self.0.join(format!("{name}.tmp"));
        std::fs::write(&tmp, command).expect("command file");
        std::fs::rename(&tmp, self.0.join(format!("{name}.json"))).expect("rename");
    }

    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&self.0)
            .expect("control directory")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

impl Drop for ControlDir {
    fn drop(&mut self) {
        lock().control_dir = None;
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn control_files_apply_in_name_order_at_the_next_call() {
    let f = fixture();
    let dir = ControlDir::new("order");
    // Reconnecting before the power loss would leave the ECU silent.
    dir.send("002", r#"{"command": "reconnect_ecu"}"#);
    dir.send(
        "001",
        r#"{"command": "inject_fault", "fault": "power_loss"}"#,
    );
    dir.send("003", r#"{"command": "inject_fault", "fault": "melt"}"#);
    // Not applied before a call into the library.
    assert_eq!(dir.files(), ["001.json", "002.json", "003.json"]);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(dir.files(), ["003.rejected"]);
    // The power loss and the reconnection.
    assert_eq!(with_ecu(|ecu| ecu.power_cycles()), 2);
    assert_eq!(
        read(f.channel, 1, 1000),
        (STATUS_NOERROR, vec![response(&[0x7E, 0x00])])
    );
}

#[test]
fn a_control_file_reaches_a_waiting_read() {
    let f = fixture();
    let dir = ControlDir::new("wait");
    let start = Instant::now();
    let path = dir.0.clone();
    let unplug = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let tmp = path.join("001.tmp");
        std::fs::write(&tmp, r#"{"command": "disconnect_vci"}"#).expect("command file");
        std::fs::rename(&tmp, path.join("001.json")).expect("rename");
    });
    let result = read(f.channel, 1, 5000);
    let elapsed = start.elapsed();
    unplug.join().expect("no panic");
    assert_eq!(result, (ERR_DEVICE_NOT_CONNECTED, vec![]));
    assert!(elapsed < Duration::from_millis(4000));
    assert!(dir.files().is_empty());
}

#[test]
fn every_call_but_get_last_error_fails_on_a_lost_device() {
    let f = fixture();
    assert_eq!(control(r#"{"command": "disconnect_vci"}"#), STATUS_NOERROR);
    assert_eq!(control(r#"{"command": "connect_vci"}"#), STATUS_NOERROR);
    let m = msg(ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    let mask = msg_bytes(&[0xFF; 4]);
    let mut out = 0;
    let mut version = [[0 as c_char; 80]; 3];
    let [fw, dll, api] = &mut version;
    let calls: [(&str, PassThruUlong); 13] = [
        ("Open", unsafe { PassThruOpen(ptr::null(), &mut out) }),
        ("Connect", unsafe {
            PassThruConnect(DEVICE_ID, ISO15765, 0, 500_000, &mut out)
        }),
        ("Disconnect", PassThruDisconnect(f.channel)),
        ("ReadMsgs", read(f.channel, 1, 0).0),
        ("ReadMsgs(null)", unsafe {
            PassThruReadMsgs(f.channel, ptr::null_mut(), ptr::null_mut(), 0)
        }),
        ("WriteMsgs(null)", unsafe {
            PassThruWriteMsgs(f.channel, ptr::null(), ptr::null_mut(), 0)
        }),
        (
            "StartPeriodicMsg",
            PassThruStartPeriodicMsg(f.channel, &m, &mut out, 100),
        ),
        ("StopPeriodicMsg", PassThruStopPeriodicMsg(f.channel, 1)),
        ("StartMsgFilter", unsafe {
            PassThruStartMsgFilter(
                f.channel,
                filter::FLOW_CONTROL_FILTER as PassThruUlong,
                &mask,
                &mask,
                &mask,
                &mut out,
            )
        }),
        ("StopMsgFilter", PassThruStopMsgFilter(f.channel, 1)),
        (
            "SetProgrammingVoltage",
            PassThruSetProgrammingVoltage(DEVICE_ID, 6, 12_000),
        ),
        ("ReadVersion", unsafe {
            PassThruReadVersion(
                DEVICE_ID,
                fw.as_mut_ptr(),
                dll.as_mut_ptr(),
                api.as_mut_ptr(),
            )
        }),
        ("Ioctl", unsafe {
            PassThruIoctl(f.channel, 0, ptr::null_mut(), ptr::null_mut())
        }),
    ];
    for (name, status) in calls {
        assert_eq!(status, ERR_DEVICE_NOT_CONNECTED, "{name}");
    }
    let mut n = 1;
    assert_eq!(
        unsafe { PassThruWriteMsgs(f.channel, &m, &mut n, 100) },
        ERR_DEVICE_NOT_CONNECTED
    );
    assert_eq!(n, 0);
    let mut text = [0 as c_char; 80];
    assert_eq!(
        unsafe { PassThruGetLastError(text.as_mut_ptr()) },
        STATUS_NOERROR
    );
    assert_eq!(PassThruClose(DEVICE_ID), ERR_DEVICE_NOT_CONNECTED);
}

#[test]
fn a_disconnect_during_a_read_reports_the_messages_already_read() {
    let f = fixture();
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    let unplug = std::thread::spawn(|| {
        std::thread::sleep(Duration::from_millis(100));
        control(r#"{"command": "disconnect_vci"}"#)
    });
    let result = read(f.channel, 2, 5000);
    assert_eq!(unplug.join().expect("no panic"), STATUS_NOERROR);
    assert_eq!(
        result,
        (ERR_DEVICE_NOT_CONNECTED, vec![response(&[0x7E, 0x00])])
    );
}

#[test]
fn a_write_to_an_unknown_channel_reports_nothing_sent() {
    let _f = fixture();
    let m = msg(ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    let mut n = 1;
    assert_eq!(
        unsafe { PassThruWriteMsgs(99, &m, &mut n, 100) },
        ERR_INVALID_CHANNEL_ID
    );
    assert_eq!(n, 0);
}

#[test]
fn a_leftover_claimed_file_is_not_picked_up() {
    let f = fixture();
    let dir = ControlDir::new("claim");
    // A file left in its claimed state, as when deleting it after applying failed, is not
    // scanned again. (That the claim happens before the apply is checked by the test with a
    // blocked claim.)
    std::fs::write(
        dir.0.join("001.applying"),
        r#"{"command": "inject_fault", "fault": "power_loss"}"#,
    )
    .expect("claimed file");
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(with_ecu(|ecu| ecu.power_cycles()), 0);
    assert_eq!(dir.files(), ["001.applying"]);
}

#[test]
fn faults_with_unknown_fields_are_rejected() {
    let _f = fixture();
    assert_eq!(
        control(
            r#"{"command": "inject_fault", "fault": {"delay_response": {"ms": 5, "extra": 1}}}"#
        ),
        ERR_FAILED
    );
}

#[test]
fn the_control_export_leaves_the_control_directory_for_the_next_call() {
    let f = fixture();
    let dir = ControlDir::new("export");
    dir.send(
        "001",
        r#"{"command": "inject_fault", "fault": "power_loss"}"#,
    );
    assert_eq!(control(r#"{"command": "reconnect_ecu"}"#), STATUS_NOERROR);
    assert_eq!(dir.files(), ["001.json"]);
    // `with_ecu` would apply the file, so the count is read without it.
    assert_eq!(lock_bus().power_cycles(), 1);
    // The power loss arrives with the next call and leaves the ECU silent.
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert!(dir.files().is_empty());
    assert_eq!(read(f.channel, 1, 20), (ERR_BUFFER_EMPTY, vec![]));
}

#[test]
fn a_control_file_that_cannot_be_claimed_holds_back_the_files_after_it() {
    let f = fixture();
    let dir = ControlDir::new("blocked");
    // A directory under the claimed name makes the rename fail on every platform.
    std::fs::create_dir(dir.0.join("001.applying")).expect("blocking directory");
    dir.send(
        "001",
        r#"{"command": "inject_fault", "fault": "power_loss"}"#,
    );
    dir.send("002", r#"{"command": "reconnect_ecu"}"#);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(dir.files(), ["001.applying", "001.json", "002.json"]);
    assert_eq!(lock_bus().power_cycles(), 0);
    // Once the claim works, both apply in order: the ECU loses power and then reconnects.
    std::fs::remove_dir(dir.0.join("001.applying")).expect("remove blocking directory");
    assert_eq!(read(f.channel, 1, 1000).0, STATUS_NOERROR);
    assert!(dir.files().is_empty());
    assert_eq!(lock_bus().power_cycles(), 2);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x3E, 0x00]);
    assert_eq!(
        read(f.channel, 1, 1000),
        (STATUS_NOERROR, vec![response(&[0x7E, 0x00])])
    );
}

// ---------------------------------------------------------------- Battery voltage

/// `READ_VBATT` on `device`: the status and the voltage written.
fn read_vbatt(device: PassThruUlong) -> (PassThruUlong, PassThruUlong) {
    let mut millivolts: PassThruUlong = 0;
    let status = unsafe {
        PassThruIoctl(
            device,
            ioctl::IOCTL_READ_VBATT as PassThruUlong,
            ptr::null_mut(),
            (&mut millivolts as *mut PassThruUlong).cast(),
        )
    };
    (status, millivolts)
}

#[test]
fn read_vbatt_reports_the_configured_voltage() {
    let _f = fixture();
    assert_eq!(read_vbatt(DEVICE_ID), (STATUS_NOERROR, 12_000));
    assert_eq!(
        control(r#"{"command": "set_battery_voltage", "millivolts": 11500}"#),
        STATUS_NOERROR
    );
    assert_eq!(read_vbatt(DEVICE_ID), (STATUS_NOERROR, 11_500));
    // Rounded to a tenth of a volt (J2534-1 7.3.3).
    assert_eq!(
        control(r#"{"command": "set_battery_voltage", "millivolts": 9349}"#),
        STATUS_NOERROR
    );
    assert_eq!(read_vbatt(DEVICE_ID), (STATUS_NOERROR, 9_300));
    assert_eq!(
        control(r#"{"command": "set_battery_voltage", "millivolts": 9350}"#),
        STATUS_NOERROR
    );
    assert_eq!(read_vbatt(DEVICE_ID), (STATUS_NOERROR, 9_400));
}

#[test]
fn read_vbatt_checks_the_device_and_the_output() {
    let _f = fixture();
    // It takes the device ID, not a channel ID: open a second channel, whose ID differs from
    // the device ID.
    let mut channel = 0;
    assert_eq!(
        unsafe { PassThruConnect(DEVICE_ID, ISO15765, 0, 500_000, &mut channel) },
        STATUS_NOERROR
    );
    assert_ne!(channel, DEVICE_ID);
    assert_eq!(read_vbatt(channel).0, ERR_INVALID_DEVICE_ID);
    assert_eq!(
        unsafe {
            PassThruIoctl(
                DEVICE_ID,
                ioctl::IOCTL_READ_VBATT as PassThruUlong,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        },
        ERR_NULL_PARAMETER
    );
    assert_eq!(PassThruClose(DEVICE_ID), STATUS_NOERROR);
    assert_eq!(read_vbatt(DEVICE_ID).0, ERR_INVALID_DEVICE_ID);
}

#[test]
fn set_battery_voltage_rejects_a_missing_or_negative_value() {
    let _f = fixture();
    for command in [
        r#"{"command": "set_battery_voltage"}"#,
        r#"{"command": "set_battery_voltage", "millivolts": -1}"#,
        r#"{"command": "set_battery_voltage", "volts": 12}"#,
    ] {
        assert_eq!(control(command), ERR_FAILED, "{command}");
    }
}

#[test]
fn read_vbatt_on_a_lost_device_reports_the_loss_before_checking_arguments() {
    let _f = fixture();
    assert_eq!(control(r#"{"command": "disconnect_vci"}"#), STATUS_NOERROR);
    assert_eq!(
        unsafe {
            PassThruIoctl(
                DEVICE_ID,
                ioctl::IOCTL_READ_VBATT as PassThruUlong,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        },
        ERR_DEVICE_NOT_CONNECTED
    );
}

// ---------------------------------------------------------------- ECU timers

#[test]
fn the_ecu_session_times_out_in_real_time() {
    // tS3 is long enough that the requests meant to land within it do so even on a busy
    // machine, and the wait meant to exceed it is well past it.
    let f = setup(EcuConfig {
        s3_server_ms: Some(1_000),
        ..default_ecu_config()
    });
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x10, 0x03]);
    assert_eq!(read(f.channel, 1, 1000).0, STATUS_NOERROR);
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(
        read(f.channel, 1, 1000),
        (STATUS_NOERROR, vec![response(&[0x62, 0xF1, 0x86, 0x03])])
    );
    // Without a request for well over tS3, the ECU is back in the default session.
    std::thread::sleep(Duration::from_millis(2_000));
    write(f.channel, ECU_PHYSICAL_REQUEST_ID, &[0x22, 0xF1, 0x86]);
    assert_eq!(
        read(f.channel, 1, 1000),
        (STATUS_NOERROR, vec![response(&[0x62, 0xF1, 0x86, 0x01])])
    );
}

// ---------------------------------------------------------------- State file (ADR-241)

/// A state file path in a fresh temporary directory, removed when dropped.
struct StateDir(PathBuf);

impl StateDir {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("sim-vci-state-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("temporary directory should be writable");
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("ecu.state")
    }
}

impl Drop for StateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_state_file_round_trips_the_ecu() {
    let dir = StateDir::new("round-trip");
    let mut ecu = SimEcu::new(default_ecu_config());
    assert!(matches!(
        ecu.request(&[0x10, 0x03]),
        sim_ecu::SimResponse::Positive(_)
    ));
    save_ecu_state(&dir.file(), &mut ecu).expect("the state should be written");
    assert!(!dir.0.join("ecu.state.tmp").exists());

    let loaded = load_ecu_state(&dir.file())
        .expect("the state should load")
        .expect("the file exists");
    assert_eq!(loaded.session, Session::Extended);
    assert_eq!(loaded.config.vin, "NGRSIMECU00000001");
}

#[test]
fn a_damaged_or_foreign_state_file_is_refused() {
    let dir = StateDir::new("refused");
    assert!(
        matches!(load_ecu_state(&dir.file()), Ok(None)),
        "no file yet"
    );
    // A directory where the file should be cannot be read, which is not the same as no file.
    std::fs::create_dir(dir.file()).expect("directory should be created");
    assert!(load_ecu_state(&dir.file()).is_err());
    std::fs::remove_dir(dir.file()).expect("directory should be removed");
    std::fs::write(dir.file(), b"not a state file").expect("file should be writable");
    assert!(load_ecu_state(&dir.file()).is_err());

    let mut ecu = SimEcu::new(default_ecu_config());
    let other_version = StateFile {
        version: STATE_FILE_VERSION + 1,
        saved_at_ms: unix_ms(SystemTime::now()),
        ecu: ecu.snapshot(),
    };
    std::fs::write(
        dir.file(),
        postcard::to_allocvec(&other_version).expect("state should encode"),
    )
    .expect("file should be writable");
    assert!(load_ecu_state(&dir.file()).is_err());
}

#[test]
fn time_since_the_write_counts_against_the_session() {
    let dir = StateDir::new("elapsed");
    let mut ecu = SimEcu::new(EcuConfig {
        s3_server_ms: Some(1_000),
        ..default_ecu_config()
    });
    assert!(matches!(
        ecu.request(&[0x10, 0x03]),
        sim_ecu::SimResponse::Positive(_)
    ));
    let mut state = StateFile {
        version: STATE_FILE_VERSION,
        saved_at_ms: unix_ms(SystemTime::now()),
        ecu: ecu.snapshot(),
    };
    // Written two seconds ago: tS3_Server ran out in the meantime.
    state.saved_at_ms -= 2_000;
    std::fs::write(
        dir.file(),
        postcard::to_allocvec(&state).expect("state should encode"),
    )
    .expect("file should be writable");
    let loaded = load_ecu_state(&dir.file())
        .expect("the state should load")
        .expect("the file exists");
    assert_eq!(loaded.session, Session::Default);
}
