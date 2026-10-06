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
