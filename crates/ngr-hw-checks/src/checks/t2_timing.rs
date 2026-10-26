//! `hw_t2_timing` (design 13.5, ADR-275): on a bench ECU, the session timing the ECU reports and
//! the round trips of a few requests through the VCI, compared with the values `sim-ecu` uses
//! (ISO 14229-2 (2021) 9.2), so the simulator's timing can be checked against a real ECU.
//!
//! The check asks for the default session (DiagnosticSessionControl, sub-function 0x01): the
//! positive response carries the server's P2 in 1 ms units and its P2* in 10 ms units (ISO
//! 14229-1 (2026) 9.2, its session parameter record), the form `sim-ecu` answers in. It then
//! sends TesterPresent (9.7) a few times. Both requests are on the guard's read-only list, so
//! the check declares no operation.
//!
//! The round trips are the agent's times: they start before the RPC and include the worker and
//! the event path, so they are reported, never judged against P2 (design 8.11 "Timing judged").
//! What is judged is the latency the VCI measured (ADR-356): from the transmit done to the
//! response's start, judged both ways, or to its end, judged one way only (a response end proves
//! the response late only for a single frame, past the limit plus one frame's transfer), each on
//! one clock. The limit is the P2 the ECU reported, beyond the timing margin the profile gives
//! its VCI (`timing_margin_us`); a profile without one gets the latencies recorded and nothing
//! judged. A reported value that differs from `sim-ecu`'s, and a late response, are recorded,
//! not failures: the check measures the ECU, it does not hold it to the simulator.

use std::time::{Duration, Instant};

use vci_service_interface::ComLogicalLinkHandle;

use crate::guard::{GuardedHandle, HandleError, VciTimes};

/// How many requests the check times: the session request and the TesterPresent requests after
/// it.
pub const SAMPLES: usize = 5;

/// The session request: DiagnosticSessionControl, default session.
const DEFAULT_SESSION: [u8; 2] = [0x10, 0x01];
/// TesterPresent with sub-function zero, whose response is not suppressed.
const TESTER_PRESENT: [u8; 2] = [0x3E, 0x00];

/// A server's session timing: P2 and P2* in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTiming {
    pub p2_ms: u16,
    pub p2_star_ms: u32,
}

impl SessionTiming {
    /// The timing `sim-ecu` reports when its configuration sets none.
    pub fn simulator() -> Self {
        Self {
            p2_ms: sim_ecu::DEFAULT_P2_SERVER_MAX_MS,
            p2_star_ms: sim_ecu::DEFAULT_P2_STAR_SERVER_MAX_MS,
        }
    }
}

/// Why the check did not pass.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// A request did not get a final response: the guard refused it, the worker failed or the
    /// ECU did not answer.
    #[error("the {request} request failed: {error}")]
    Request {
        request: &'static str,
        #[source]
        error: HandleError,
    },
    /// The ECU refused a request (a negative response with this code).
    #[error("the ECU refused the {request} request with negative response code {nrc:#04x}")]
    Negative { request: &'static str, nrc: u8 },
    /// The ECU answered a request with something that is not its positive response.
    #[error("the {request} response {response:02X?} is not the positive response")]
    Malformed {
        request: &'static str,
        response: Vec<u8>,
    },
}

/// Which two times a latency was taken between (design 8.11 `measured_by`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeasuredBy {
    /// The VCI's transmit done to the response's start, on one clock.
    VciTxDoneToRxStart,
    /// The VCI's transmit done to the response's end, on one clock.
    VciTxDoneToRxEnd,
}

impl MeasuredBy {
    /// The name design 8.11 gives it.
    pub fn name(self) -> &'static str {
        match self {
            Self::VciTxDoneToRxStart => "vci_tx_done_to_rx_start",
            Self::VciTxDoneToRxEnd => "vci_tx_done_to_rx_end",
        }
    }
}

/// What the check concludes from a request's VCI times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Judgement {
    /// The response came within P2 plus the margin.
    WithinP2,
    /// The response came past P2 plus the margin (for a response end, plus one frame's
    /// transfer too), by this much.
    Late(Duration),
    /// No rule decides, for this reason.
    NotJudged(&'static str),
}

/// One request the check timed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    pub request: &'static str,
    /// The agent's round trip, reported only.
    pub round_trip: Duration,
    /// The VCI times of the exchange, as the link's events gave them.
    pub times: VciTimes,
    /// The latency the VCI measured, when two of its times are on one clock.
    pub latency: Option<(Duration, MeasuredBy)>,
    pub judgement: Judgement,
}

/// What the check measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingOutcome {
    /// What the ECU reported.
    pub reported: SessionTiming,
    /// What `sim-ecu` uses.
    pub simulator: SessionTiming,
    /// The profile's timing margin for its VCI, if it gives one.
    pub margin: Option<Duration>,
    /// Each request, the session request first.
    pub samples: Vec<Sample>,
}

impl TimingOutcome {
    /// Whether the ECU reported the timing `sim-ecu` uses.
    pub fn matches_simulator(&self) -> bool {
        self.reported == self.simulator
    }

    /// The agent's round trip of each request, the session request first.
    pub fn round_trips(&self) -> Vec<Duration> {
        self.samples
            .iter()
            .map(|sample| sample.round_trip)
            .collect()
    }

    /// The outcome as the report's message.
    pub fn summary(&self) -> String {
        let mark = |same: bool| if same { "" } else { ", differs" };
        let round_trips = self.round_trips();
        let min = round_trips.iter().map(millis).fold(f64::INFINITY, f64::min);
        let max = round_trips.iter().map(millis).fold(0.0, f64::max);
        let margin = match self.margin {
            Some(margin) => format!(
                "judged against P2 plus a margin of {:.3} ms",
                millis(&margin)
            ),
            None => "the profile gives no timing margin, so nothing is judged".to_owned(),
        };
        let vci = self
            .samples
            .iter()
            .map(|sample| {
                let measured = match sample.latency {
                    Some((latency, by)) => format!("{:.3} ms ({})", millis(&latency), by.name()),
                    None => "no latency".to_owned(),
                };
                let judged = match sample.judgement {
                    Judgement::WithinP2 => "within P2".to_owned(),
                    Judgement::Late(by) => format!("late by {:.3} ms", millis(&by)),
                    Judgement::NotJudged(why) => format!("not judged: {why}"),
                };
                format!("{} {measured}, {judged}", sample.request)
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "the ECU reports P2 {} ms (sim-ecu {} ms{}) and P2* {} ms (sim-ecu {} ms{}); the \
             agent's round trips of {} requests took {min:.1} to {max:.1} ms (agent times, not \
             judged against P2); the latencies the VCI measured, {margin}: {vci}",
            self.reported.p2_ms,
            self.simulator.p2_ms,
            mark(self.reported.p2_ms == self.simulator.p2_ms),
            self.reported.p2_star_ms,
            self.simulator.p2_star_ms,
            mark(self.reported.p2_star_ms == self.simulator.p2_star_ms),
            round_trips.len(),
        )
    }
}

fn millis(duration: &Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The longest response a single CAN frame carries under every addressing format the profile
/// may give (ISO 15765-2 (2024) 9.6.2: one byte of protocol information, and one more for an
/// address byte): a longer one may span frames.
const SINGLE_FRAME_MAX: usize = 6;

/// An upper bound on the bits of one classic CAN frame of eight data bytes, extended identifier
/// and worst-case bit stuffing included.
const FRAME_BITS_MAX: u64 = 160;

/// How long one CAN frame takes at most at `baud_rate`; none for a rate of zero.
fn frame_transfer(baud_rate: u32) -> Option<Duration> {
    (baud_rate > 0)
        .then(|| Duration::from_micros((FRAME_BITS_MAX * 1_000_000).div_ceil(u64::from(baud_rate))))
}

/// The latency the VCI measured for one exchange and what it shows (design 8.11 "Timing
/// judged"): from the transmit done to the response's start where both are on one clock, judged
/// both ways, else to its end, judged one way only; `response_len` bytes of response, a limit of
/// `p2` and the profile's `margin`, on a link of `baud_rate`.
pub fn judge(
    times: &VciTimes,
    response_len: usize,
    p2: Duration,
    margin: Option<Duration>,
    baud_rate: Option<u32>,
) -> (Option<(Duration, MeasuredBy)>, Judgement) {
    let Some(tx_done) = times.tx_done else {
        return (None, Judgement::NotJudged("the VCI gave no transmit time"));
    };
    let micros = |us: u32| Duration::from_micros(u64::from(us));
    let start = times
        .rx_start
        .and_then(|start| tx_done.until(start))
        .map(|us| (micros(us), MeasuredBy::VciTxDoneToRxStart));
    let end = times
        .rx_end
        .and_then(|end| tx_done.until(end))
        .map(|us| (micros(us), MeasuredBy::VciTxDoneToRxEnd));
    let Some((latency, by)) = start.or(end) else {
        return (
            None,
            Judgement::NotJudged("the VCI gave no response time on the transmit's clock"),
        );
    };
    let Some(margin) = margin else {
        return (
            Some((latency, by)),
            Judgement::NotJudged("the profile gives no timing margin"),
        );
    };
    let limit = p2 + margin;
    let judgement = if latency <= limit {
        Judgement::WithinP2
    } else if by == MeasuredBy::VciTxDoneToRxStart {
        Judgement::Late(latency - limit)
    } else if response_len > SINGLE_FRAME_MAX {
        Judgement::NotJudged(
            "a response end past the limit does not prove a multi-frame response late",
        )
    } else {
        match baud_rate.and_then(frame_transfer) {
            Some(transfer) if latency > limit + transfer => {
                Judgement::Late(latency - limit - transfer)
            }
            Some(_) => Judgement::NotJudged(
                "the response end lies within one frame's transfer past the limit",
            ),
            None => Judgement::NotJudged("the link's baud rate is not known"),
        }
    };
    (Some((latency, by)), judgement)
}

/// The session timing a DiagnosticSessionControl response for the default session reports:
/// P2 in 1 ms units, then P2* in 10 ms units, each two bytes, most significant first.
pub fn session_timing(response: &[u8]) -> Result<SessionTiming, CheckError> {
    const REQUEST: &str = "DiagnosticSessionControl";
    match *response {
        [0x50, 0x01, p2_high, p2_low, star_high, star_low] => Ok(SessionTiming {
            p2_ms: u16::from_be_bytes([p2_high, p2_low]),
            p2_star_ms: u32::from(u16::from_be_bytes([star_high, star_low])) * 10,
        }),
        [0x7F, 0x10, nrc] => Err(CheckError::Negative {
            request: REQUEST,
            nrc,
        }),
        _ => Err(CheckError::Malformed {
            request: REQUEST,
            response: response.to_vec(),
        }),
    }
}

/// Runs the check on `cll`, a link of `handle` to the profile's ECU.
pub async fn run(
    handle: &mut GuardedHandle,
    cll: ComLogicalLinkHandle,
) -> Result<TimingOutcome, CheckError> {
    let margin = handle.timing_margin();
    let baud_rate = handle.baud_rate(cll).ok();
    let (response, first) =
        timed(handle, cll, "DiagnosticSessionControl", &DEFAULT_SESSION).await?;
    let reported = session_timing(&response)?;
    let p2 = Duration::from_millis(u64::from(reported.p2_ms));
    let sample = |request, response: &[u8], (round_trip, times): (Duration, VciTimes)| {
        let (latency, judgement) = judge(&times, response.len(), p2, margin, baud_rate);
        Sample {
            request,
            round_trip,
            times,
            latency,
            judgement,
        }
    };
    let mut samples = vec![sample("DiagnosticSessionControl", &response, first)];
    for _ in 1..SAMPLES {
        const REQUEST: &str = "TesterPresent";
        let (response, took) = timed(handle, cll, REQUEST, &TESTER_PRESENT).await?;
        match *response {
            [0x7E, 0x00] => {}
            [0x7F, 0x3E, nrc] => {
                return Err(CheckError::Negative {
                    request: REQUEST,
                    nrc,
                });
            }
            _ => {
                return Err(CheckError::Malformed {
                    request: REQUEST,
                    response,
                });
            }
        }
        samples.push(sample(REQUEST, &response, took));
    }
    Ok(TimingOutcome {
        reported,
        simulator: SessionTiming::simulator(),
        margin,
        samples,
    })
}

/// Sends `request` and returns its final response with the agent's round trip and the VCI times
/// of the exchange.
async fn timed(
    handle: &mut GuardedHandle,
    cll: ComLogicalLinkHandle,
    name: &'static str,
    request: &[u8],
) -> Result<(Vec<u8>, (Duration, VciTimes)), CheckError> {
    let start = Instant::now();
    let timed = handle
        .send_receive_timed(cll, request)
        .await
        .map_err(|error| CheckError::Request {
            request: name,
            error,
        })?;
    Ok((timed.response, (start.elapsed(), timed.times)))
}

#[cfg(test)]
#[path = "t2_timing_tests.rs"]
mod tests;
