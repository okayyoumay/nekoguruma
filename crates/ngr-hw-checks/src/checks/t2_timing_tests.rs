use std::time::Duration;

use sim_ecu::{EcuConfig, SimEcu, SimResponse};
use vci_service_interface::TimestampClock;

use super::*;
use crate::guard::VciTime;

/// What `sim-ecu` with `config` answers to the check's session request, as the bytes on the wire.
fn simulator_answer(config: EcuConfig) -> Vec<u8> {
    match SimEcu::new(config).request(&DEFAULT_SESSION) {
        SimResponse::Positive(bytes) => bytes,
        other => panic!("sim-ecu answers the default session positively, not {other:?}"),
    }
}

fn sample(round_trip_us: u64) -> Sample {
    Sample {
        request: "TesterPresent",
        round_trip: Duration::from_micros(round_trip_us),
        times: VciTimes::default(),
        latency: None,
        judgement: Judgement::NotJudged("the VCI gave no transmit time"),
    }
}

fn outcome(reported: SessionTiming) -> TimingOutcome {
    TimingOutcome {
        reported,
        simulator: SessionTiming::simulator(),
        margin: None,
        samples: vec![sample(12_300), sample(4_100)],
    }
}

/// A VCI time on the device's clock.
fn device(us: u32) -> Option<VciTime> {
    Some(VciTime {
        us,
        clock: Some(TimestampClock::Device),
    })
}

const P2: Duration = Duration::from_millis(25);
const MARGIN: Option<Duration> = Some(Duration::from_micros(500));
const LIMITS: Limits = Limits {
    p2: P2,
    margin: MARGIN,
    baud_rate: Some(500_000),
    forwards_pendings: true,
};

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
         round trips of 2 requests took 4.1 to 12.3 ms (agent times, not judged against P2); the \
         latencies the VCI measured, the profile gives no timing margin, so nothing is judged: \
         TesterPresent no latency, not judged: the VCI gave no transmit time; TesterPresent no \
         latency, not judged: the VCI gave no transmit time"
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

#[test]
fn the_latency_runs_from_the_transmit_to_the_response_start_and_is_judged_both_ways() {
    let times = VciTimes {
        tx_done: device(1_000),
        rx_start: device(4_000),
        rx_end: device(9_000),
        pending_seen: false,
    };
    let (latency, judgement) = judge(&times, 2, &LIMITS);
    assert_eq!(
        latency,
        Some((Duration::from_micros(3_000), MeasuredBy::VciTxDoneToRxStart))
    );
    assert_eq!(judgement, Judgement::WithinP2);
    // A start past P2 plus the margin proves the response late, by how much.
    let late = VciTimes {
        rx_start: device(1_000 + 25_000 + 500 + 1_200),
        ..times
    };
    assert_eq!(
        judge(&late, 2, &LIMITS).1,
        Judgement::Late(Duration::from_micros(1_200))
    );
    // At the limit is within it.
    let at_limit = VciTimes {
        rx_start: device(1_000 + 25_500),
        ..times
    };
    assert_eq!(judge(&at_limit, 2, &LIMITS).1, Judgement::WithinP2);
    // Without a margin the latency is recorded and nothing judged.
    let (latency, judgement) = judge(
        &late,
        2,
        &Limits {
            margin: None,
            ..LIMITS
        },
    );
    assert!(latency.is_some());
    assert_eq!(
        judgement,
        Judgement::NotJudged("the profile gives no timing margin")
    );
}

#[test]
fn a_response_end_is_judged_one_way_only() {
    // A library that refused the indications leaves the transmit done on the response and its
    // end (ISO 22900-2 (2022) 10.1.4.11.4), or only the end.
    let end = |us| VciTimes {
        tx_done: device(0),
        rx_start: None,
        rx_end: device(us),
        pending_seen: false,
    };
    let (latency, judgement) = judge(&end(20_000), 2, &LIMITS);
    assert_eq!(
        latency,
        Some((Duration::from_micros(20_000), MeasuredBy::VciTxDoneToRxEnd))
    );
    assert_eq!(judgement, Judgement::WithinP2);
    // One frame takes at most 320 us at 500 kbit/s: an end within that past the limit proves
    // nothing, one past it proves a single-frame response late.
    assert_eq!(
        judge(&end(25_500 + 320), 2, &LIMITS).1,
        Judgement::NotJudged("the response end lies within one frame's transfer past the limit")
    );
    assert_eq!(
        judge(&end(25_500 + 320 + 80), 2, &LIMITS).1,
        Judgement::Late(Duration::from_micros(80))
    );
    // A response that may span frames is never late by its end, and nor is one on a link whose
    // baud rate is not known.
    assert_eq!(
        judge(&end(40_000), 7, &LIMITS).1,
        Judgement::NotJudged(
            "a response end past the limit does not prove a multi-frame response late"
        )
    );
    assert_eq!(
        judge(
            &end(40_000),
            2,
            &Limits {
                baud_rate: None,
                ..LIMITS
            }
        )
        .1,
        Judgement::NotJudged("the link's baud rate is not known")
    );
}

#[test]
fn times_on_different_clocks_or_with_none_are_not_judged() {
    let worker = Some(VciTime {
        us: 4_000,
        clock: Some(TimestampClock::Worker),
    });
    let unclocked = Some(VciTime {
        us: 4_000,
        clock: None,
    });
    for later in [worker, unclocked, device(500)] {
        let times = VciTimes {
            tx_done: device(1_000),
            rx_start: later,
            rx_end: later,
            pending_seen: false,
        };
        assert_eq!(
            judge(&times, 2, &LIMITS),
            (
                None,
                Judgement::NotJudged("the VCI gave no response time on the transmit's clock")
            ),
            "{later:?}"
        );
    }
    // A start on another clock falls back to an end on the transmit's.
    let times = VciTimes {
        tx_done: device(1_000),
        rx_start: worker,
        rx_end: device(5_000),
        pending_seen: false,
    };
    assert_eq!(
        judge(&times, 2, &LIMITS).0,
        Some((Duration::from_micros(4_000), MeasuredBy::VciTxDoneToRxEnd))
    );
    assert_eq!(
        judge(&VciTimes::default(), 2, &LIMITS),
        (None, Judgement::NotJudged("the VCI gave no transmit time"))
    );
}

#[test]
fn the_summary_names_each_latency_with_what_measured_it() {
    let mut judged = outcome(SessionTiming::simulator());
    judged.margin = MARGIN;
    judged.samples[0].latency =
        Some((Duration::from_micros(3_200), MeasuredBy::VciTxDoneToRxStart));
    judged.samples[0].judgement = Judgement::WithinP2;
    judged.samples[1].latency = Some((Duration::from_micros(30_000), MeasuredBy::VciTxDoneToRxEnd));
    judged.samples[1].judgement = Judgement::Late(Duration::from_micros(4_180));
    let summary = judged.summary();
    assert!(
        summary.ends_with(
            "the latencies the VCI measured, judged against P2 plus a margin of 0.500 ms: \
             TesterPresent 3.200 ms (vci_tx_done_to_rx_start), within P2; TesterPresent 30.000 ms \
             (vci_tx_done_to_rx_end), late by 4.180 ms"
        ),
        "{summary}"
    );
}

#[test]
fn past_p2_a_pending_or_a_link_that_may_absorb_one_leaves_the_response_unjudged() {
    let late = VciTimes {
        tx_done: device(0),
        rx_start: device(40_000),
        rx_end: device(41_000),
        pending_seen: false,
    };
    assert_eq!(
        judge(&late, 6, &LIMITS).1,
        Judgement::Late(Duration::from_micros(14_500))
    );
    // A pending passed on moves the limit in force to P2*, which the check does not judge.
    let after_pending = VciTimes {
        pending_seen: true,
        ..late
    };
    assert_eq!(
        judge(&after_pending, 6, &LIMITS).1,
        Judgement::NotJudged("a response pending moved the limit in force to P2*")
    );
    // A link that may not pass every pending on may have hidden one.
    let absorbing = Limits {
        forwards_pendings: false,
        ..LIMITS
    };
    assert_eq!(
        judge(&late, 6, &absorbing).1,
        Judgement::NotJudged("the VCI may have absorbed a response pending")
    );
    // Within P2 the response is on time either way.
    let on_time = VciTimes {
        rx_start: device(20_000),
        ..after_pending
    };
    assert_eq!(judge(&on_time, 6, &absorbing).1, Judgement::WithinP2);
}

#[test]
fn the_session_response_s_six_bytes_are_one_frame() {
    // DiagnosticSessionControl's positive response fits one frame under every addressing format,
    // so its end past the limit and one frame's transfer proves it late.
    let end = VciTimes {
        tx_done: device(0),
        rx_start: None,
        rx_end: device(25_500 + 320 + 1),
        pending_seen: false,
    };
    assert_eq!(
        judge(&end, 6, &LIMITS).1,
        Judgement::Late(Duration::from_micros(1))
    );
}
