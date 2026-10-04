//! Each service has a positive case and at least one NRC case, checked against its clause in
//! ISO 14229-1 (2026 edition).

use super::*;

fn config() -> EcuConfig {
    EcuConfig {
        vin: "WVWZZZ1KZAW000001".into(),
        part_number: "1K0907115".into(),
        sw_version: "0042".into(),
        require_security_access: true,
        dtcs: vec![
            DtcRecord {
                dtc: 0x01_23_45,
                status: 0x09,
            },
            DtcRecord {
                dtc: 0xC1_00_00,
                status: 0x50,
            },
        ],
        ..Default::default()
    }
}

fn ecu() -> SimEcu {
    SimEcu::new(config())
}

fn pos(bytes: &[u8]) -> SimResponse {
    SimResponse::Positive(bytes.to_vec())
}

fn neg(sid: u8, nrc: Nrc) -> SimResponse {
    SimResponse::Negative { sid, nrc }
}

fn enter(ecu: &mut SimEcu, session: Session) {
    let r = ecu.request(&[0x10, session.code()]);
    assert!(matches!(r, SimResponse::Positive(_)), "{r:?}");
}

fn unlock(ecu: &mut SimEcu) {
    let SimResponse::Positive(seed) = ecu.request(&[0x27, 0x01]) else {
        panic!("requestSeed failed");
    };
    let seed = u32::from_be_bytes([seed[2], seed[3], seed[4], seed[5]]);
    let key = SimEcu::key_for_seed(seed).to_be_bytes();
    assert_eq!(
        ecu.request(&[0x27, 0x02, key[0], key[1], key[2], key[3]]),
        pos(&[0x67, 0x02])
    );
}

/// Programming session, unlocked, flash erased.
fn ready_to_download() -> SimEcu {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Programming);
    unlock(&mut ecu);
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0xFF, 0x00]),
        pos(&[0x71, 0x01, 0xFF, 0x00])
    );
    ecu
}

/// RequestDownload of `size` bytes at `address` (4-byte address, 4-byte size).
fn request_download(address: u32, size: u32) -> Vec<u8> {
    let mut msg = vec![0x34, 0x00, 0x44];
    msg.extend_from_slice(&address.to_be_bytes());
    msg.extend_from_slice(&size.to_be_bytes());
    msg
}

fn transfer(bsc: u8, data: &[u8]) -> Vec<u8> {
    let mut msg = vec![0x36, bsc];
    msg.extend_from_slice(data);
    msg
}

// ---------------------------------------------------------------- 7.7 general behaviour

#[test]
fn unsupported_service_is_rejected_with_sns() {
    assert_eq!(
        ecu().request(&[0x85, 0x01]),
        neg(0x85, Nrc::ServiceNotSupported)
    );
}

#[test]
fn service_outside_its_session_is_rejected_with_snsias() {
    assert_eq!(
        ecu().request(&[0x27, 0x01]),
        neg(0x27, Nrc::ServiceNotSupportedInActiveSession)
    );
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    assert_eq!(
        ecu.request(&request_download(0, 16)),
        neg(0x34, Nrc::ServiceNotSupportedInActiveSession)
    );
}

#[test]
fn functional_addressing_suppresses_sns_sfns_and_roor() {
    let mut ecu = ecu();
    let f = Addressing::Functional;
    assert_eq!(ecu.request_with(f, &[0x85, 0x01]), SimResponse::NoResponse);
    assert_eq!(ecu.request_with(f, &[0x3E, 0x05]), SimResponse::NoResponse);
    assert_eq!(
        ecu.request_with(f, &[0x22, 0x12, 0x34]),
        SimResponse::NoResponse
    );
    // Other NRCs are still sent.
    assert_eq!(
        ecu.request_with(f, &[0x3E]),
        neg(0x3E, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(ecu.request_with(f, &[0x3E, 0x00]), pos(&[0x7E, 0x00]));
}

#[test]
fn suppress_bit_hides_only_positive_responses() {
    let mut ecu = ecu();
    assert_eq!(ecu.request(&[0x3E, 0x80]), SimResponse::NoResponse);
    assert_eq!(
        ecu.request(&[0x3E, 0x81]),
        neg(0x3E, Nrc::SubFunctionNotSupported)
    );
}

#[test]
fn gateway_authentication_is_checked_before_the_session() {
    let mut ecu = SimEcu::new(EcuConfig {
        require_gateway_auth: true,
        ..config()
    });
    assert_eq!(
        ecu.request(&request_download(0, 16)),
        neg(0x34, Nrc::AuthenticationRequired)
    );
    ecu.gateway_authenticated = true;
    assert_eq!(
        ecu.request(&request_download(0, 16)),
        neg(0x34, Nrc::ServiceNotSupportedInActiveSession)
    );
}

#[test]
fn negative_response_wire_format() {
    assert_eq!(
        neg(0x22, Nrc::RequestOutOfRange).to_bytes(),
        Some(vec![0x7F, 0x22, 0x31])
    );
    assert_eq!(SimResponse::NoResponse.to_bytes(), None);
}

// ---------------------------------------------------------------- 0x10 (9.2)

#[test]
fn session_control_switches_session_and_reports_timing() {
    let mut ecu = ecu();
    assert_eq!(
        ecu.request(&[0x10, 0x03]),
        pos(&[0x50, 0x03, 0x00, 0x32, 0x01, 0xF4])
    );
    assert_eq!(ecu.session, Session::Extended);
}

#[test]
fn session_control_rejects_unknown_session_and_bad_length() {
    let mut ecu = ecu();
    assert_eq!(
        ecu.request(&[0x10, 0x04]),
        neg(0x10, Nrc::SubFunctionNotSupported)
    );
    assert_eq!(
        ecu.request(&[0x10, 0x03, 0x00]),
        neg(0x10, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(
        ecu.request(&[0x10]),
        neg(0x10, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(ecu.session, Session::Default);
}

#[test]
fn session_restart_relocks_security() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    unlock(&mut ecu);
    enter(&mut ecu, Session::Extended);
    assert!(!ecu.security_unlocked);
}

// ---------------------------------------------------------------- 0x11 (9.3)

#[test]
fn ecu_reset_answers_then_returns_to_default_session() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    unlock(&mut ecu);
    assert_eq!(ecu.request(&[0x11, 0x01]), pos(&[0x51, 0x01]));
    assert_eq!(ecu.session, Session::Default);
    assert!(!ecu.security_unlocked);
}

#[test]
fn ecu_reset_rejects_rapid_power_shutdown_and_bad_length() {
    let mut ecu = ecu();
    assert_eq!(
        ecu.request(&[0x11, 0x04]),
        neg(0x11, Nrc::SubFunctionNotSupported)
    );
    assert_eq!(
        ecu.request(&[0x11, 0x01, 0x00]),
        neg(0x11, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
}

// ---------------------------------------------------------------- 0x27 (9.4)

#[test]
fn security_access_unlocks_with_the_right_key() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    unlock(&mut ecu);
    assert!(ecu.security_unlocked);
    // An unlocked level answers requestSeed with an all-zero seed.
    assert_eq!(ecu.request(&[0x27, 0x01]), pos(&[0x67, 0x01, 0, 0, 0, 0]));
}

#[test]
fn security_access_seeds_are_deterministic_and_never_zero() {
    let seeds = |mut ecu: SimEcu| -> Vec<SimResponse> {
        enter(&mut ecu, Session::Extended);
        (0..3).map(|_| ecu.request(&[0x27, 0x01])).collect()
    };
    let a = seeds(ecu());
    assert_eq!(a, seeds(ecu()));
    for r in &a {
        let SimResponse::Positive(bytes) = r else {
            panic!("{r:?}")
        };
        assert_ne!(&bytes[2..], &[0, 0, 0, 0]);
    }
}

#[test]
fn security_access_send_key_without_seed_is_a_sequence_error() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    assert_eq!(
        ecu.request(&[0x27, 0x02, 0, 0, 0, 0]),
        neg(0x27, Nrc::RequestSequenceError)
    );
}

#[test]
fn security_access_wrong_keys_start_the_delay_timer() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    for attempt in 1..=MAX_SECURITY_ATTEMPTS {
        assert!(matches!(
            ecu.request(&[0x27, 0x01]),
            SimResponse::Positive(_)
        ));
        let expected = if attempt < MAX_SECURITY_ATTEMPTS {
            Nrc::InvalidKey
        } else {
            Nrc::ExceededNumberOfAttempts
        };
        assert_eq!(ecu.request(&[0x27, 0x02, 0, 0, 0, 0]), neg(0x27, expected));
    }
    assert_eq!(
        ecu.request(&[0x27, 0x01]),
        neg(0x27, Nrc::RequiredTimeDelayNotExpired)
    );
    ecu.expire_security_delay();
    unlock(&mut ecu);
}

#[test]
fn security_access_rejects_other_levels_and_bad_length() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    assert_eq!(
        ecu.request(&[0x27, 0x03]),
        neg(0x27, Nrc::SubFunctionNotSupported)
    );
    assert_eq!(
        ecu.request(&[0x27, 0x01, 0xAA]),
        neg(0x27, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert!(matches!(
        ecu.request(&[0x27, 0x01]),
        SimResponse::Positive(_)
    ));
    assert_eq!(
        ecu.request(&[0x27, 0x02, 0x00]),
        neg(0x27, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    // The malformed sendKey used up the seed (Annex I, transition 9).
    assert_eq!(
        ecu.request(&[0x27, 0x02, 0, 0, 0, 0]),
        neg(0x27, Nrc::RequestSequenceError)
    );
    // Without a seed, sendKey is out of sequence whatever its length (Annex I, transition 4).
    assert_eq!(
        ecu.request(&[0x27, 0x02, 0x00]),
        neg(0x27, Nrc::RequestSequenceError)
    );
}

#[test]
fn ecu_reset_clears_the_false_attempt_counter() {
    let mut ecu = ecu();
    for _ in 1..MAX_SECURITY_ATTEMPTS {
        enter(&mut ecu, Session::Extended);
        ecu.request(&[0x27, 0x01]);
        ecu.request(&[0x27, 0x02, 0, 0, 0, 0]);
    }
    assert_eq!(ecu.request(&[0x11, 0x01]), pos(&[0x51, 0x01]));
    enter(&mut ecu, Session::Extended);
    ecu.request(&[0x27, 0x01]);
    assert_eq!(
        ecu.request(&[0x27, 0x02, 0, 0, 0, 0]),
        neg(0x27, Nrc::InvalidKey)
    );
}

// ---------------------------------------------------------------- 0x3E (9.7)

#[test]
fn tester_present_answers_zero_sub_function() {
    assert_eq!(ecu().request(&[0x3E, 0x00]), pos(&[0x7E, 0x00]));
}

#[test]
fn tester_present_rejects_other_sub_functions_and_bad_length() {
    let mut ecu = ecu();
    assert_eq!(
        ecu.request(&[0x3E, 0x01]),
        neg(0x3E, Nrc::SubFunctionNotSupported)
    );
    assert_eq!(
        ecu.request(&[0x3E, 0x00, 0x00]),
        neg(0x3E, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
}

// ---------------------------------------------------------------- 0x22 (10.2)

#[test]
fn read_data_by_identifier_returns_each_supported_did() {
    let mut ecu = ecu();
    let mut expected = vec![0x62, 0xF1, 0x90];
    expected.extend_from_slice(b"WVWZZZ1KZAW000001");
    expected.extend_from_slice(&[0xF1, 0x86, 0x01]);
    // The unsupported 0x1234 is left out.
    assert_eq!(
        ecu.request(&[0x22, 0xF1, 0x90, 0x12, 0x34, 0xF1, 0x86]),
        pos(&expected)
    );
}

#[test]
fn read_data_by_identifier_nrcs() {
    let mut ecu = ecu();
    assert_eq!(
        ecu.request(&[0x22, 0x12, 0x34]),
        neg(0x22, Nrc::RequestOutOfRange)
    );
    assert_eq!(
        ecu.request(&[0x22, 0xF1]),
        neg(0x22, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(
        ecu.request(&[0x22, 0xF1, 0x90, 0xF1]),
        neg(0x22, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    let too_many: Vec<u8> = std::iter::once(0x22)
        .chain([0xF1, 0x86].repeat(MAX_DIDS_PER_READ + 1))
        .collect();
    assert_eq!(
        ecu.request(&too_many),
        neg(0x22, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
}

#[test]
fn read_data_by_identifier_response_too_long() {
    let mut ecu = SimEcu::new(EcuConfig {
        vin: "X".repeat(1000),
        ..config()
    });
    let req: Vec<u8> = std::iter::once(0x22)
        .chain([0xF1, 0x90].repeat(5))
        .collect();
    assert_eq!(ecu.request(&req), neg(0x22, Nrc::ResponseTooLong));
}

// ---------------------------------------------------------------- 0x2E (10.7)

fn write_vin(vin: &[u8]) -> Vec<u8> {
    let mut msg = vec![0x2E, 0xF1, 0x90];
    msg.extend_from_slice(vin);
    msg
}

#[test]
fn write_data_by_identifier_writes_the_vin() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    unlock(&mut ecu);
    assert_eq!(
        ecu.request(&write_vin(b"1HGCM82633A004352")),
        pos(&[0x6E, 0xF1, 0x90])
    );
    assert_eq!(ecu.config.vin, "1HGCM82633A004352");
}

#[test]
fn write_data_by_identifier_nrcs() {
    let mut ecu = ecu();
    // Not writable in the default session.
    assert_eq!(
        ecu.request(&write_vin(b"1HGCM82633A004352")),
        neg(0x2E, Nrc::RequestOutOfRange)
    );
    enter(&mut ecu, Session::Extended);
    assert_eq!(
        ecu.request(&write_vin(b"1HGCM82633A004352")),
        neg(0x2E, Nrc::SecurityAccessDenied)
    );
    assert_eq!(
        ecu.request(&[0x2E, 0xF1, 0x87, 0x00]),
        neg(0x2E, Nrc::RequestOutOfRange)
    );
    assert_eq!(
        ecu.request(&write_vin(b"SHORT")),
        neg(0x2E, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(
        ecu.request(&[0x2E, 0xF1, 0x90]),
        neg(0x2E, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    unlock(&mut ecu);
    assert_eq!(
        ecu.request(&write_vin(b"1HGCM82633A00435-")),
        neg(0x2E, Nrc::RequestOutOfRange)
    );
    assert_eq!(ecu.config.vin, "WVWZZZ1KZAW000001");
}

// ---------------------------------------------------------------- 0x14 (11.2)

#[test]
fn clear_diagnostic_information_resets_status_bits() {
    let mut ecu = ecu();
    assert_eq!(ecu.request(&[0x14, 0xFF, 0xFF, 0xFF]), pos(&[0x54]));
    assert!(ecu.dtcs.iter().all(|r| r.status == DTC_STATUS_AFTER_CLEAR));
}

#[test]
fn clear_diagnostic_information_single_dtc() {
    let mut ecu = ecu();
    assert_eq!(ecu.request(&[0x14, 0x01, 0x23, 0x45]), pos(&[0x54]));
    assert_eq!(ecu.dtcs[0].status, DTC_STATUS_AFTER_CLEAR);
}

#[test]
fn clear_diagnostic_information_nrcs() {
    let mut ecu = ecu();
    assert_eq!(
        ecu.request(&[0x14, 0x99, 0x99, 0x99]),
        neg(0x14, Nrc::RequestOutOfRange)
    );
    // MemorySelection is not supported.
    assert_eq!(
        ecu.request(&[0x14, 0xFF, 0xFF, 0xFF, 0x01]),
        neg(0x14, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(ecu.dtcs[0].status, 0x09);
}

// ---------------------------------------------------------------- 0x19 (11.3)

#[test]
fn read_dtc_information_by_status_mask() {
    let mut ecu = ecu();
    // Mask 0x08 (confirmedDTC) matches only the first DTC.
    assert_eq!(
        ecu.request(&[0x19, 0x02, 0x08]),
        pos(&[0x59, 0x02, 0xFF, 0x01, 0x23, 0x45, 0x09])
    );
    assert_eq!(
        ecu.request(&[0x19, 0x01, 0x08]),
        pos(&[0x59, 0x01, 0xFF, 0x01, 0x00, 0x01])
    );
    assert_eq!(
        ecu.request(&[0x19, 0x0A]),
        pos(&[
            0x59, 0x0A, 0xFF, 0x01, 0x23, 0x45, 0x09, 0xC1, 0x00, 0x00, 0x50
        ])
    );
}

#[test]
fn read_dtc_information_nrcs() {
    let mut ecu = ecu();
    assert_eq!(
        ecu.request(&[0x19, 0x06, 0x01, 0x23, 0x45, 0xFF]),
        neg(0x19, Nrc::SubFunctionNotSupported)
    );
    assert_eq!(
        ecu.request(&[0x19, 0x02]),
        neg(0x19, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(
        ecu.request(&[0x19, 0x0A, 0xFF]),
        neg(0x19, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
}

// ---------------------------------------------------------------- 0x31 (13.2)

#[test]
fn routine_control_erase_and_check() {
    let mut ecu = ready_to_download();
    assert_eq!(ecu.flash, FlashPhase::Erased);
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0xFF, 0x01]),
        neg(0x31, Nrc::RequestSequenceError)
    );
    assert!(matches!(
        ecu.request(&request_download(0, 4)),
        SimResponse::Positive(_)
    ));
    assert_eq!(ecu.request(&transfer(1, &[1, 2, 3, 4])), pos(&[0x76, 0x01]));
    assert_eq!(ecu.request(&[0x37]), pos(&[0x77]));
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0xFF, 0x01]),
        pos(&[0x71, 0x01, 0xFF, 0x01, 0x00])
    );
    assert_eq!(ecu.flash, FlashPhase::Verified);
}

#[test]
fn routine_control_check_reports_a_bad_image() {
    let mut ecu = SimEcu::new(EcuConfig {
        fail_checksum: true,
        ..config()
    });
    enter(&mut ecu, Session::Programming);
    unlock(&mut ecu);
    ecu.request(&[0x31, 0x01, 0xFF, 0x00]);
    ecu.request(&request_download(0, 1));
    ecu.request(&transfer(1, &[0xAA]));
    ecu.request(&[0x37]);
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0xFF, 0x01]),
        pos(&[0x71, 0x01, 0xFF, 0x01, 0x01])
    );
    assert_eq!(ecu.flash, FlashPhase::TransferComplete);
}

#[test]
fn routine_control_nrcs() {
    let mut ecu = ecu();
    // Routines exist only in the programming session.
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0xFF, 0x00]),
        neg(0x31, Nrc::RequestOutOfRange)
    );
    enter(&mut ecu, Session::Programming);
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0xFF, 0x00]),
        neg(0x31, Nrc::SecurityAccessDenied)
    );
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0x12, 0x34]),
        neg(0x31, Nrc::RequestOutOfRange)
    );
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0xFF]),
        neg(0x31, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    unlock(&mut ecu);
    assert_eq!(
        ecu.request(&[0x31, 0x02, 0xFF, 0x00]),
        neg(0x31, Nrc::SubFunctionNotSupported)
    );
}

// ---------------------------------------------------------------- 0x34 / 0x36 / 0x37 (14.2, 14.4, 14.5)

#[test]
fn download_transfer_exit_stores_the_image() {
    let mut ecu = ready_to_download();
    assert_eq!(
        ecu.request(&request_download(0x100, 6)),
        pos(&[0x74, 0x20, 0x01, 0x02])
    );
    assert_eq!(ecu.flash, FlashPhase::Transferring { next_block: 1 });
    assert_eq!(ecu.request(&transfer(1, &[1, 2, 3])), pos(&[0x76, 0x01]));
    assert_eq!(ecu.request(&transfer(2, &[4, 5, 6])), pos(&[0x76, 0x02]));
    assert_eq!(ecu.request(&[0x37]), pos(&[0x77]));
    assert_eq!(ecu.image(), &[1, 2, 3, 4, 5, 6]);
    assert_eq!(ecu.flash, FlashPhase::TransferComplete);
}

#[test]
fn request_download_nrcs() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Programming);
    assert_eq!(
        ecu.request(&[0x34, 0x00, 0x11, 0x00]),
        neg(0x34, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(
        ecu.request(&[0x34, 0x10, 0x11, 0x00, 0x10]),
        neg(0x34, Nrc::RequestOutOfRange)
    );
    assert_eq!(
        ecu.request(&[0x34, 0x00, 0x05, 0x00, 0x10]),
        neg(0x34, Nrc::RequestOutOfRange)
    );
    assert_eq!(
        ecu.request(&[0x34, 0x00, 0x22, 0x00, 0x10]),
        neg(0x34, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(
        ecu.request(&request_download(FLASH_SIZE - 4, 8)),
        neg(0x34, Nrc::RequestOutOfRange)
    );
    assert_eq!(
        ecu.request(&request_download(0, 0)),
        neg(0x34, Nrc::RequestOutOfRange)
    );
    assert_eq!(
        ecu.request(&request_download(0, 16)),
        neg(0x34, Nrc::SecurityAccessDenied)
    );
    unlock(&mut ecu);
    // The flash has not been erased.
    assert_eq!(
        ecu.request(&request_download(0, 16)),
        neg(0x34, Nrc::ConditionsNotCorrect)
    );
}

#[test]
fn request_download_while_transferring_is_rejected() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 16));
    assert_eq!(
        ecu.request(&request_download(0, 16)),
        neg(0x34, Nrc::ConditionsNotCorrect)
    );
    // Erasing during a transfer is refused too.
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0xFF, 0x00]),
        neg(0x31, Nrc::ConditionsNotCorrect)
    );
}

#[test]
fn transfer_data_nrcs() {
    let mut ecu = ready_to_download();
    assert_eq!(
        ecu.request(&transfer(1, &[0])),
        neg(0x36, Nrc::RequestSequenceError)
    );
    ecu.request(&request_download(0, 4));
    assert_eq!(
        ecu.request(&[0x36, 0x01]),
        neg(0x36, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    let oversized = transfer(1, &vec![0; usize::from(MAX_BLOCK_LENGTH) - 1]);
    assert_eq!(
        ecu.request(&oversized),
        neg(0x36, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
    assert_eq!(
        ecu.request(&transfer(1, &[0; 5])),
        neg(0x36, Nrc::TransferDataSuspended)
    );
    assert_eq!(
        ecu.request(&transfer(2, &[0; 2])),
        neg(0x36, Nrc::WrongBlockSequenceCounter)
    );
    assert_eq!(ecu.request(&transfer(1, &[0; 4])), pos(&[0x76, 0x01]));
    assert_eq!(
        ecu.request(&transfer(2, &[0])),
        neg(0x36, Nrc::RequestSequenceError)
    );
}

#[test]
fn transfer_data_accepts_a_repeated_block_once() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 4));
    assert_eq!(ecu.request(&transfer(1, &[1, 2])), pos(&[0x76, 0x01]));
    assert_eq!(ecu.request(&transfer(1, &[1, 2])), pos(&[0x76, 0x01]));
    assert_eq!(ecu.request(&transfer(2, &[3, 4])), pos(&[0x76, 0x02]));
    assert_eq!(ecu.image(), &[1, 2, 3, 4]);
}

#[test]
fn transfer_data_block_sequence_counter_wraps_to_zero() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 256));
    for block in 1..=256u32 {
        let bsc = block as u8;
        assert_eq!(ecu.request(&transfer(bsc, &[bsc])), pos(&[0x76, bsc]));
    }
    assert_eq!(ecu.request(&[0x37]), pos(&[0x77]));
    assert_eq!(ecu.image().len(), 256);
}

#[test]
fn request_transfer_exit_nrcs() {
    let mut ecu = ready_to_download();
    assert_eq!(ecu.request(&[0x37]), neg(0x37, Nrc::RequestSequenceError));
    ecu.request(&request_download(0, 4));
    ecu.request(&transfer(1, &[0; 2]));
    assert_eq!(ecu.request(&[0x37]), neg(0x37, Nrc::RequestSequenceError));
    ecu.request(&transfer(2, &[0; 2]));
    assert_eq!(
        ecu.request(&[0x37, 0x00]),
        neg(0x37, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
}

// ---------------------------------------------------------------- Interruption and resume (8.2.5)

#[test]
fn dropped_block_interrupts_and_the_transfer_resumes() {
    let mut ecu = SimEcu::new(EcuConfig {
        drop_at_block: Some(3),
        ..config()
    });
    enter(&mut ecu, Session::Programming);
    unlock(&mut ecu);
    ecu.request(&[0x31, 0x01, 0xFF, 0x00]);
    ecu.request(&request_download(0x40, 8));
    assert_eq!(ecu.request(&transfer(1, &[1, 2])), pos(&[0x76, 0x01]));
    assert_eq!(ecu.request(&transfer(2, &[3, 4])), pos(&[0x76, 0x02]));
    assert_eq!(ecu.request(&transfer(3, &[5, 6])), SimResponse::NoResponse);
    // The ECU stays silent until it is reconnected.
    assert_eq!(ecu.request(&[0x3E, 0x00]), SimResponse::NoResponse);

    // The injected drop fires once.
    assert_eq!(ecu.config.drop_at_block, None);
    ecu.reconnect();
    assert_eq!(ecu.session, Session::Default);
    assert_eq!(
        ecu.request(&[0x22, 0xFD, 0x00]),
        pos(&[0x62, 0xFD, 0x00, 0x05, 0, 0, 0, 2, 0, 0, 0, 4])
    );

    enter(&mut ecu, Session::Programming);
    unlock(&mut ecu);
    // Only the part not yet received can be requested.
    assert_eq!(
        ecu.request(&request_download(0x40, 8)),
        neg(0x34, Nrc::ConditionsNotCorrect)
    );
    assert!(matches!(
        ecu.request(&request_download(0x44, 4)),
        SimResponse::Positive(_)
    ));
    assert_eq!(ecu.flash, FlashPhase::Transferring { next_block: 3 });
    // blockSequenceCounter restarts at 1 after RequestDownload.
    assert_eq!(ecu.request(&transfer(1, &[5, 6])), pos(&[0x76, 0x01]));
    assert_eq!(ecu.request(&transfer(2, &[7, 8])), pos(&[0x76, 0x02]));
    assert_eq!(ecu.request(&[0x37]), pos(&[0x77]));
    assert_eq!(ecu.image(), &[1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn leaving_the_programming_session_interrupts_the_transfer() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 4));
    ecu.request(&transfer(1, &[0; 2]));
    enter(&mut ecu, Session::Default);
    assert_eq!(ecu.flash, FlashPhase::Interrupted { last_block: 1 });
}

fn read_flash_state(ecu: &mut SimEcu) -> Vec<u8> {
    let SimResponse::Positive(bytes) = ecu.request(&[0x22, 0xFD, 0x00]) else {
        panic!("FD00 read failed");
    };
    bytes[3..].to_vec()
}

/// Programming session and unlocked again after an interruption.
fn back_in_programming(ecu: &mut SimEcu) {
    enter(ecu, Session::Programming);
    unlock(ecu);
}

// ---------------------------------------------------------------- Review follow-ups

#[test]
fn any_failed_security_request_discards_the_pending_seed() {
    for bad in [&[0x27, 0x01, 0xAA][..], &[0x27, 0x03], &[0x27]] {
        let mut ecu = ecu();
        enter(&mut ecu, Session::Extended);
        let SimResponse::Positive(seed) = ecu.request(&[0x27, 0x01]) else {
            panic!("requestSeed failed");
        };
        assert!(matches!(ecu.request(bad), SimResponse::Negative { .. }));
        let seed = u32::from_be_bytes([seed[2], seed[3], seed[4], seed[5]]);
        let key = SimEcu::key_for_seed(seed).to_be_bytes();
        assert_eq!(
            ecu.request(&[0x27, 0x02, key[0], key[1], key[2], key[3]]),
            neg(0x27, Nrc::RequestSequenceError),
            "after {bad:02X?}"
        );
    }
}

#[test]
fn successful_unlock_resets_the_false_attempt_counter() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    let fail = |ecu: &mut SimEcu| {
        ecu.request(&[0x27, 0x01]);
        ecu.request(&[0x27, 0x02, 0, 0, 0, 0])
    };
    for _ in 1..MAX_SECURITY_ATTEMPTS {
        assert_eq!(fail(&mut ecu), neg(0x27, Nrc::InvalidKey));
    }
    unlock(&mut ecu);
    enter(&mut ecu, Session::Extended);
    for _ in 1..MAX_SECURITY_ATTEMPTS {
        assert_eq!(fail(&mut ecu), neg(0x27, Nrc::InvalidKey));
    }
}

#[test]
fn suppressed_positive_responses_still_change_state() {
    let mut ecu = ecu();
    assert_eq!(ecu.request(&[0x10, 0x82]), SimResponse::NoResponse);
    assert_eq!(ecu.session, Session::Programming);

    let SimResponse::Positive(seed) = ecu.request(&[0x27, 0x01]) else {
        panic!("requestSeed failed");
    };
    let seed = u32::from_be_bytes([seed[2], seed[3], seed[4], seed[5]]);
    let key = SimEcu::key_for_seed(seed).to_be_bytes();
    assert_eq!(
        ecu.request(&[0x27, 0x82, key[0], key[1], key[2], key[3]]),
        SimResponse::NoResponse
    );
    assert!(ecu.security_unlocked);

    assert_eq!(
        ecu.request(&[0x31, 0x81, 0xFF, 0x00]),
        SimResponse::NoResponse
    );
    assert_eq!(ecu.flash, FlashPhase::Erased);

    assert_eq!(ecu.request(&[0x11, 0x81]), SimResponse::NoResponse);
    assert_eq!(ecu.session, Session::Default);
}

#[test]
fn functional_addressing_suppresses_snsias() {
    assert_eq!(
        ecu().request_with(Addressing::Functional, &[0x27, 0x01]),
        SimResponse::NoResponse
    );
}

#[test]
fn routine_control_checks_security_before_sub_function() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Programming);
    assert_eq!(
        ecu.request(&[0x31, 0x02, 0xFF, 0x00]),
        neg(0x31, Nrc::SecurityAccessDenied)
    );
}

#[test]
fn request_transfer_exit_checks_sequence_before_length() {
    let mut ecu = ready_to_download();
    assert_eq!(
        ecu.request(&[0x37, 0x00]),
        neg(0x37, Nrc::RequestSequenceError)
    );
}

#[test]
fn read_dtc_information_mask_matches_any_common_bit() {
    let mut ecu = ecu();
    // 0x41 shares bit 0 with the first DTC (0x09) and bit 6 with the second (0x50).
    assert_eq!(
        ecu.request(&[0x19, 0x01, 0x41]),
        pos(&[0x59, 0x01, 0xFF, 0x01, 0x00, 0x02])
    );
    assert_eq!(
        ecu.request(&[0x19, 0x01, 0x20]),
        pos(&[0x59, 0x01, 0xFF, 0x01, 0x00, 0x00])
    );
}

#[test]
fn read_data_by_identifier_boundaries() {
    let mut ecu = SimEcu::new(EcuConfig {
        vin: "V".repeat(MAX_RESPONSE_LENGTH - 3),
        ..config()
    });
    // Eight DIDs is the limit, not over it.
    let eight: Vec<u8> = std::iter::once(0x22)
        .chain([0xF1, 0x86].repeat(MAX_DIDS_PER_READ))
        .collect();
    assert!(matches!(ecu.request(&eight), SimResponse::Positive(_)));
    // SID + DID + record is exactly the longest response.
    let SimResponse::Positive(bytes) = ecu.request(&[0x22, 0xF1, 0x90]) else {
        panic!("VIN read failed");
    };
    assert_eq!(bytes.len(), MAX_RESPONSE_LENGTH);
}

#[test]
fn write_data_by_identifier_rejects_a_long_vin() {
    let mut ecu = ecu();
    enter(&mut ecu, Session::Extended);
    unlock(&mut ecu);
    assert_eq!(
        ecu.request(&write_vin(b"1HGCM82633A0043521")),
        neg(0x2E, Nrc::IncorrectMessageLengthOrInvalidFormat)
    );
}

#[test]
fn request_download_accepts_the_end_of_flash() {
    let mut ecu = ready_to_download();
    assert!(matches!(
        ecu.request(&request_download(FLASH_START + FLASH_SIZE - 8, 8)),
        SimResponse::Positive(_)
    ));
}

#[test]
fn transfer_data_accepts_a_block_of_max_length() {
    let mut ecu = ready_to_download();
    let data_len = usize::from(MAX_BLOCK_LENGTH) - 2;
    ecu.request(&request_download(0, data_len as u32));
    assert_eq!(
        ecu.request(&transfer(1, &vec![0x5A; data_len])),
        pos(&[0x76, 0x01])
    );
}

#[test]
fn transfer_data_checks_memory_size_before_the_counter() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 4));
    assert_eq!(
        ecu.request(&transfer(2, &[0; 5])),
        neg(0x36, Nrc::TransferDataSuspended)
    );
}

#[test]
fn transfer_data_repeat_of_the_final_block_is_accepted() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 2));
    assert_eq!(ecu.request(&transfer(1, &[1, 2])), pos(&[0x76, 0x01]));
    assert_eq!(ecu.request(&transfer(1, &[1, 2])), pos(&[0x76, 0x01]));
    assert_eq!(ecu.request(&[0x37]), pos(&[0x77]));
}

#[test]
fn transfer_data_repeat_with_different_data_is_rejected() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 8));
    ecu.request(&transfer(1, &[1, 2]));
    assert_eq!(
        ecu.request(&transfer(1, &[9, 9, 9, 9])),
        neg(0x36, Nrc::WrongBlockSequenceCounter)
    );
    assert_eq!(ecu.image(), &[1, 2]);
}

#[test]
fn flash_state_did_reports_each_phase() {
    let mut ecu = ready_to_download();
    assert_eq!(read_flash_state(&mut ecu), [0x01, 0, 0, 0, 0, 0, 0, 0, 0]);
    ecu.request(&request_download(0, 4));
    ecu.request(&transfer(1, &[0; 3]));
    assert_eq!(read_flash_state(&mut ecu), [0x02, 0, 0, 0, 2, 0, 0, 0, 3]);
    ecu.request(&transfer(2, &[0]));
    ecu.request(&[0x37]);
    assert_eq!(read_flash_state(&mut ecu), [0x03, 0, 0, 0, 0, 0, 0, 0, 0]);
    ecu.request(&[0x31, 0x01, 0xFF, 0x01]);
    assert_eq!(read_flash_state(&mut ecu), [0x04, 0, 0, 0, 0, 0, 0, 0, 0]);
}

#[test]
fn resume_requires_both_the_remaining_address_and_size() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0x40, 8));
    ecu.request(&transfer(1, &[1, 2]));
    ecu.reconnect();
    back_in_programming(&mut ecu);
    assert_eq!(
        ecu.request(&request_download(0x42, 8)),
        neg(0x34, Nrc::ConditionsNotCorrect)
    );
    assert_eq!(
        ecu.request(&request_download(0x40, 6)),
        neg(0x34, Nrc::ConditionsNotCorrect)
    );
    assert!(matches!(
        ecu.request(&request_download(0x42, 6)),
        SimResponse::Positive(_)
    ));
}

#[test]
fn resume_after_ecu_reset_restarts_the_counter_at_one() {
    // One block stored with counter 1; after the resume, counter 1 is a new block, not a repeat.
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 4));
    ecu.request(&transfer(1, &[1, 2]));
    assert_eq!(ecu.request(&[0x11, 0x01]), pos(&[0x51, 0x01]));
    assert_eq!(ecu.flash, FlashPhase::Interrupted { last_block: 1 });
    back_in_programming(&mut ecu);
    ecu.request(&request_download(2, 2));
    assert_eq!(ecu.request(&transfer(1, &[3, 4])), pos(&[0x76, 0x01]));
    assert_eq!(ecu.request(&[0x37]), pos(&[0x77]));
    assert_eq!(ecu.image(), &[1, 2, 3, 4]);
}

#[test]
fn interrupted_after_the_last_block_can_still_exit() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 4));
    ecu.request(&transfer(1, &[1, 2, 3, 4]));
    ecu.reconnect();
    back_in_programming(&mut ecu);
    assert_eq!(read_flash_state(&mut ecu), [0x05, 0, 0, 0, 1, 0, 0, 0, 4]);
    assert_eq!(ecu.request(&[0x37]), pos(&[0x77]));
    assert_eq!(ecu.flash, FlashPhase::TransferComplete);
}

#[test]
fn erase_from_interrupted_discards_the_partial_image() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 8));
    ecu.request(&transfer(1, &[1, 2]));
    ecu.reconnect();
    back_in_programming(&mut ecu);
    assert_eq!(
        ecu.request(&[0x31, 0x01, 0xFF, 0x00]),
        pos(&[0x71, 0x01, 0xFF, 0x00])
    );
    assert_eq!(ecu.flash, FlashPhase::Erased);
    assert!(ecu.image().is_empty());
    assert_eq!(ecu.request(&[0x37]), neg(0x37, Nrc::RequestSequenceError));
}

#[test]
fn restarting_the_programming_session_interrupts_the_transfer() {
    let mut ecu = ready_to_download();
    ecu.request(&request_download(0, 4));
    ecu.request(&transfer(1, &[0; 2]));
    enter(&mut ecu, Session::Programming);
    assert_eq!(ecu.flash, FlashPhase::Interrupted { last_block: 1 });
}

#[test]
fn reconnect_clears_gateway_authentication() {
    let mut ecu = ecu();
    ecu.gateway_authenticated = true;
    ecu.reconnect();
    assert!(!ecu.gateway_authenticated);
}
