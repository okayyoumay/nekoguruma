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
//! A reported value that differs from `sim-ecu`'s is recorded, not a failure: the check measures
//! the ECU, it does not hold it to the simulator.

use std::time::{Duration, Instant};

use vci_service_interface::ComLogicalLinkHandle;

use crate::guard::{GuardedHandle, HandleError};

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

/// What the check measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingOutcome {
    /// What the ECU reported.
    pub reported: SessionTiming,
    /// What `sim-ecu` uses.
    pub simulator: SessionTiming,
    /// The agent's round trip of each request, the session request first.
    pub round_trips: Vec<Duration>,
}

impl TimingOutcome {
    /// Whether the ECU reported the timing `sim-ecu` uses.
    pub fn matches_simulator(&self) -> bool {
        self.reported == self.simulator
    }

    /// The outcome as the report's message.
    pub fn summary(&self) -> String {
        let mark = |same: bool| if same { "" } else { ", differs" };
        let millis = |d: &Duration| d.as_secs_f64() * 1000.0;
        let min = self
            .round_trips
            .iter()
            .map(millis)
            .fold(f64::INFINITY, f64::min);
        let max = self.round_trips.iter().map(millis).fold(0.0, f64::max);
        format!(
            "the ECU reports P2 {} ms (sim-ecu {} ms{}) and P2* {} ms (sim-ecu {} ms{}); the \
             agent's round trips of {} requests took {min:.1} to {max:.1} ms (agent times, not \
             judged against P2)",
            self.reported.p2_ms,
            self.simulator.p2_ms,
            mark(self.reported.p2_ms == self.simulator.p2_ms),
            self.reported.p2_star_ms,
            self.simulator.p2_star_ms,
            mark(self.reported.p2_star_ms == self.simulator.p2_star_ms),
            self.round_trips.len(),
        )
    }
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
    let (response, first) =
        timed(handle, cll, "DiagnosticSessionControl", &DEFAULT_SESSION).await?;
    let reported = session_timing(&response)?;
    let mut round_trips = vec![first];
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
        round_trips.push(took);
    }
    Ok(TimingOutcome {
        reported,
        simulator: SessionTiming::simulator(),
        round_trips,
    })
}

/// Sends `request` and returns its final response with the agent's round trip.
async fn timed(
    handle: &mut GuardedHandle,
    cll: ComLogicalLinkHandle,
    name: &'static str,
    request: &[u8],
) -> Result<(Vec<u8>, Duration), CheckError> {
    let start = Instant::now();
    let response =
        handle
            .send_receive(cll, request)
            .await
            .map_err(|error| CheckError::Request {
                request: name,
                error,
            })?;
    Ok((response, start.elapsed()))
}

#[cfg(test)]
#[path = "t2_timing_tests.rs"]
mod tests;
