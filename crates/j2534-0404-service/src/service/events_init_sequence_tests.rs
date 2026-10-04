use super::*;

fn params_with(value: Option<u32>) -> ComParamSet {
    let mut p = ComParamSet::default();
    if let Some(v) = value {
        p.unum32.insert(PARAM_INIT_SETTINGS, v);
    }
    p
}

#[test]
fn explicit_value_1_selects_five_baud_regardless_of_protocol_or_data_len() {
    let p = params_with(Some(1));
    assert_eq!(
        select_init_sequence(&p, j2534_0404::ISO14230, &[0x68, 0x6A, 0xF1, 0x81], 0),
        InitSequence::FiveBaud
    );
}

#[test]
fn explicit_value_2_selects_fast_regardless_of_protocol_or_data_len() {
    let p = params_with(Some(2));
    assert_eq!(
        select_init_sequence(&p, j2534_0404::ISO9141, &[0x33], 0),
        InitSequence::Fast
    );
}

#[test]
fn explicit_value_3_selects_none() {
    let p = params_with(Some(3));
    assert_eq!(
        select_init_sequence(&p, j2534_0404::ISO14230, &[0x33], 0),
        InitSequence::None
    );
}

#[test]
fn absent_param_falls_back_to_legacy_heuristic() {
    let p = params_with(None);
    // ISO9141 always five-baud under the legacy heuristic.
    assert_eq!(
        select_init_sequence(&p, j2534_0404::ISO9141, &[0x68, 0x6A, 0xF1, 0x81], 0),
        InitSequence::FiveBaud
    );
    // ISO14230 with a single init byte -- five-baud.
    assert_eq!(
        select_init_sequence(&p, j2534_0404::ISO14230, &[0x33], 0),
        InitSequence::FiveBaud
    );
    // ISO14230 with more than one init byte -- fast-init.
    assert_eq!(
        select_init_sequence(&p, j2534_0404::ISO14230, &[0x68, 0x6A, 0xF1, 0x81], 0),
        InitSequence::Fast
    );
}

#[test]
fn out_of_range_value_falls_back_to_legacy_heuristic() {
    let p = params_with(Some(9));
    assert_eq!(
        select_init_sequence(&p, j2534_0404::ISO9141, &[0x33], 0),
        InitSequence::FiveBaud
    );
}

/// ADR-170 Decision 8 (design-advisor fix): clause 12.3.2/12.3.4.2 defines
/// only 5-baud init for UART_ECHO_BYTE_PS, so the legacy heuristic must
/// select `FiveBaud` unconditionally for it, the same way it does for
/// ISO9141 -- unlike ISO14230, empty `init_data` must NOT fall through to
/// `Fast` for this protocol (clause 12 never defines a fast-init).
#[test]
fn uart_echo_byte_ps_with_empty_init_data_selects_five_baud_under_legacy_heuristic() {
    let p = params_with(None);
    assert_eq!(
        select_init_sequence(&p, j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS, &[], 0),
        InitSequence::FiveBaud
    );
}

/// Companion to the empty-`init_data` case above: an oversized `init_data`
/// (which would select `Fast` for ISO14230) must also still select
/// `FiveBaud` for UART_ECHO_BYTE_PS -- clause 12 never defines a FAST_INIT
/// for this protocol.
#[test]
fn uart_echo_byte_ps_with_four_byte_init_data_selects_five_baud_under_legacy_heuristic() {
    let p = params_with(None);
    assert_eq!(
        select_init_sequence(
            &p,
            j2534_0404::PROTOCOL_UART_ECHO_BYTE_PS,
            &[0x68, 0x6A, 0xF1, 0x81],
            0
        ),
        InitSequence::FiveBaud
    );
}
