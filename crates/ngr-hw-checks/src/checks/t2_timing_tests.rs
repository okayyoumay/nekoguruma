use std::time::Duration;

use sim_ecu::{EcuConfig, SimEcu, SimResponse};

use super::*;

/// What `sim-ecu` with `config` answers to the check's session request, as the bytes on the wire.
fn simulator_answer(config: EcuConfig) -> Vec<u8> {
    match SimEcu::new(config).request(&DEFAULT_SESSION) {
        SimResponse::Positive(bytes) => bytes,
        other => panic!("sim-ecu answers the default session positively, not {other:?}"),
    }
}

fn outcome(reported: SessionTiming) -> TimingOutcome {
    TimingOutcome {
        reported,
        simulator: SessionTiming::simulator(),
        round_trips: vec![Duration::from_micros(12_300), Duration::from_micros(4_100)],
    }
}

#[test]
fn the_simulator_s_own_answer_reads_as_its_timing() {
    // The comparison holds `sim-ecu` to the values it reports itself, read as the check reads a
    // real ECU's answer.
    let reported = session_timing(&simulator_answer(EcuConfig::default())).unwrap();
    assert_eq!(reported, SessionTiming::simulator());
    assert_eq!(
        reported,
        SessionTiming {
            p2_ms: 50,
            p2_star_ms: 5_000
        }
    );
    assert!(outcome(reported).matches_simulator());
    // A configured simulator reports its own values, and the comparison shows them as different.
    let configured = session_timing(&simulator_answer(EcuConfig {
        p2_server_max_ms: Some(25),
        p2_star_server_max_ms: Some(2_000),
        ..EcuConfig::default()
    }))
    .unwrap();
    assert_eq!(
        configured,
        SessionTiming {
            p2_ms: 25,
            p2_star_ms: 2_000
        }
    );
    assert!(!outcome(configured).matches_simulator());
}

#[test]
fn the_session_parameter_record_reads_p2_in_milliseconds_and_p2_star_in_tens() {
    assert_eq!(
        session_timing(&[0x50, 0x01, 0x12, 0x34, 0xFF, 0xFF]).unwrap(),
        SessionTiming {
            p2_ms: 0x1234,
            p2_star_ms: 655_350
        }
    );
}

#[test]
fn a_refusal_or_another_answer_is_not_a_timing() {
    assert!(matches!(
        session_timing(&[0x7F, 0x10, 0x22]),
        Err(CheckError::Negative { nrc: 0x22, .. })
    ));
    for response in [
        &[][..],
        &[0x50, 0x01],
        &[0x50, 0x03, 0x00, 0x32, 0x01, 0xF4],
        &[0x50, 0x01, 0x00, 0x32, 0x01, 0xF4, 0x00],
        &[0x7F, 0x22, 0x31],
    ] {
        assert!(
            matches!(session_timing(response), Err(CheckError::Malformed { .. })),
            "{response:02X?}"
        );
    }
}

#[test]
fn the_summary_names_each_value_beside_the_simulator_s_and_the_round_trips_as_unjudged() {
    let same = outcome(SessionTiming::simulator()).summary();
    assert_eq!(
        same,
        "the ECU reports P2 50 ms (sim-ecu 50 ms) and P2* 5000 ms (sim-ecu 5000 ms); the agent's \
         round trips of 2 requests took 4.1 to 12.3 ms (agent times, not judged against P2)"
    );
    let differs = outcome(SessionTiming {
        p2_ms: 25,
        p2_star_ms: 5_000,
    })
    .summary();
    assert!(
        differs.starts_with(
            "the ECU reports P2 25 ms (sim-ecu 50 ms, differs) and P2* 5000 ms (sim-ecu 5000 ms);"
        ),
        "{differs}"
    );
}
