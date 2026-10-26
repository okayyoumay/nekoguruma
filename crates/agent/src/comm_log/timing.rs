//! The judgement of an exchange's times against the limit in force (design 8.11 "Timing
//! judged", "Limit in force", "Possible pending", `timing`).
//!
//! Every time compared is a VCI time placed on its clock (ADR-361), and a difference is taken
//! only between two times on one clock and epoch, from the transmit done. A response start and
//! the VCI's receive timeout are judged both ways; a response end only as proof that the
//! response was on time, since it includes the response's own transfer. On a link whose VCI
//! absorbs pendings the limit that depends on them is bounded from the transmit: the first
//! pending came within P2, and the 0x78 completion limit runs from it.

use super::clocks::Placed;
use super::exchange::{Clock, MeasuredBy, NotJudgedReason, Timing, Window};

/// The limits of a link, as `link_opened` records them, and its timing margin, in
/// microseconds. A completion limit of 0 is none (`link::LinkConfig::validate`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub p2_us: u64,
    pub p2_star_us: u64,
    pub completion_us: Option<u64>,
    pub margin_us: u64,
}

/// A time placed on its clock.
pub(crate) type At = Option<(Clock, Placed)>;

/// The VCI times an exchange's timing is judged from.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Times<'a> {
    pub transmit: At,
    /// The pendings seen, in order.
    pub pendings: &'a [At],
    pub start: At,
    pub end: At,
    /// The VCI's receive timeout, when it ended the exchange.
    pub timeout: At,
}

/// What the times show against the limits, on a link that forwards pendings (`absorbing`
/// false) or absorbs them.
pub(crate) fn judge(limits: Limits, absorbing: bool, times: Times<'_>) -> Timing {
    let mut timing = Timing {
        p2_ms: Some(ms_ceil(limits.p2_us)),
        p2_star_ms: Some(ms_ceil(limits.p2_star_us)),
        completion_ms: limits.completion_us.map(ms_ceil),
        margin_ms: Some(ms_ceil(limits.margin_us)),
        ..Timing::not_judged()
    };
    let Some((clock, transmit)) = times.transmit else {
        return timing;
    };
    // A time on the transmit's clock and epoch, in microseconds from the transmit.
    let since = |at: At| {
        let (own, at) = at?;
        (own == clock && at.epoch == transmit.epoch).then_some(at.unwrapped - transmit.unwrapped)
    };
    let response = since(times.start)
        .map(|us| (us, MeasuredBy::VciTxDoneToRxStart))
        .or_else(|| since(times.end).map(|us| (us, MeasuredBy::VciTxDoneToRxEnd)));
    if let Some((us, measured_by)) = response {
        timing.response_ms = Some(ms(us));
        timing.measured_by = measured_by;
        if let Some(latest) = times.pendings.last().and_then(|at| since(*at)) {
            timing.since_latest_pending_ms = Some(ms(us - latest));
        }
    }
    let margin = to_i64(limits.margin_us);
    let p2 = to_i64(limits.p2_us);
    // The limit in force, in microseconds from the transmit: P2 until a pending, then the
    // earlier of P2* from the latest and the completion limit from the first. `None` when a
    // pending seen is not on the transmit's clock and epoch.
    let limit = if absorbing || times.pendings.is_empty() {
        Some(p2)
    } else {
        let placed: Option<Vec<i64>> = times.pendings.iter().map(|at| since(*at)).collect();
        placed.map(|pendings| {
            let latest = pendings.last().copied().unwrap_or_default() + to_i64(limits.p2_star_us);
            match limits.completion_us {
                Some(completion) => latest.min(pendings[0] + to_i64(completion)),
                None => latest,
            }
        })
    };
    // On a link whose VCI absorbs pendings, the latest an unseen chain can end: a first pending
    // within P2 and the completion limit from it. None without a completion limit.
    let bound = limits
        .completion_us
        .map(|completion| p2 + to_i64(completion));
    let within = if times.pendings.is_empty() {
        Window::WithinP2
    } else {
        Window::WithinP2Star
    };
    let set = |timing: &mut Timing, window: Window, by_us: Option<i64>| {
        timing.window = window;
        timing.window_by_ms = by_us.map(ms);
        timing.not_judged_reason = None;
    };
    let not_judged = |timing: &mut Timing, reason: NotJudgedReason| {
        timing.window = Window::NotJudged;
        timing.not_judged_reason = Some(reason);
    };
    let Some(limit) = limit else {
        not_judged(&mut timing, NotJudgedReason::OtherClock);
        return timing;
    };
    if let Some(timeout) = since(times.timeout) {
        if timeout < limit - margin {
            set(&mut timing, Window::TimeoutEarly, Some(limit - timeout));
        } else if timeout <= limit + margin {
            set(&mut timing, Window::TimeoutAtLimit, None);
        } else if !absorbing {
            set(&mut timing, Window::TimeoutLate, Some(timeout - limit));
        } else {
            match bound {
                Some(bound) if timeout > bound + margin => {
                    set(&mut timing, Window::TimeoutLate, Some(timeout - bound))
                }
                _ => set(&mut timing, Window::PossiblePending, None),
            }
        }
    } else if let Some(start) = since(times.start) {
        if start <= limit + margin {
            set(&mut timing, within, None);
        } else if !absorbing {
            set(&mut timing, Window::Late, Some(start - limit));
        } else {
            match bound {
                Some(bound) if start > bound + margin => {
                    set(&mut timing, Window::Late, Some(start - bound))
                }
                _ => set(&mut timing, Window::PossiblePending, None),
            }
        }
    } else if let Some(end) = since(times.end) {
        // An end within the limit proves the response on time; one past it proves nothing,
        // since the end includes the response's own transfer.
        if end <= limit + margin {
            set(&mut timing, within, None);
        } else {
            not_judged(&mut timing, NotJudgedReason::EndAfterLimit);
        }
    } else if times.start.is_some() || times.end.is_some() || times.timeout.is_some() {
        not_judged(&mut timing, NotJudgedReason::OtherClock);
    } else {
        not_judged(&mut timing, NotJudgedReason::NoTimeToJudge);
    }
    timing
}

fn to_i64(us: u64) -> i64 {
    i64::try_from(us).unwrap_or(i64::MAX)
}

fn ms(us: i64) -> f64 {
    us as f64 / 1000.0
}

/// Whole milliseconds, rounded up, as `link_opened` gives a timing ComParam.
fn ms_ceil(us: u64) -> u32 {
    u32::try_from(us.div_ceil(1000)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P2 50 ms, P2* 5 s, a completion limit of 25 s and a 2 ms margin.
    const LIMITS: Limits = Limits {
        p2_us: 50_000,
        p2_star_us: 5_000_000,
        completion_us: Some(25_000_000),
        margin_us: 2_000,
    };

    /// A device time `us` microseconds after a transmit at 1 000 000, epoch 0.
    fn at(us: i64) -> At {
        on(0, 1_000_000 + us)
    }

    fn on(epoch: u32, unwrapped: i64) -> At {
        Some((Clock::Device, Placed { epoch, unwrapped }))
    }

    fn transmit() -> At {
        at(0)
    }

    fn judged(absorbing: bool, times: Times<'_>) -> (Window, Option<f64>, Option<NotJudgedReason>) {
        let timing = judge(LIMITS, absorbing, times);
        (timing.window, timing.window_by_ms, timing.not_judged_reason)
    }

    #[test]
    fn the_limits_are_reported_in_milliseconds() {
        let timing = judge(LIMITS, false, Times::default());
        assert_eq!(
            (
                timing.p2_ms,
                timing.p2_star_ms,
                timing.completion_ms,
                timing.margin_ms
            ),
            (Some(50), Some(5_000), Some(25_000), Some(2))
        );
        assert_eq!(timing.window, Window::NotJudged);
        assert_eq!(
            timing.not_judged_reason,
            Some(NotJudgedReason::NoTransmitTime)
        );
    }

    #[test]
    fn a_start_is_judged_both_ways() {
        let start = |us| Times {
            transmit: transmit(),
            start: at(us),
            end: at(us + 3_000),
            ..Times::default()
        };
        // Within P2 plus the margin; the start is what is measured.
        let timing = judge(LIMITS, false, start(51_999));
        assert_eq!(timing.window, Window::WithinP2);
        assert_eq!(timing.measured_by, MeasuredBy::VciTxDoneToRxStart);
        assert_eq!(timing.response_ms, Some(51.999));
        assert_eq!(judged(false, start(52_000)).0, Window::WithinP2);
        // Past it: late, by how much past P2.
        assert_eq!(
            judged(false, start(52_001)),
            (Window::Late, Some(2.001), None)
        );
    }

    #[test]
    fn after_a_pending_the_limit_is_p2_star_or_the_completion_limit() {
        let pendings = [at(10_000), at(4_000_000)];
        let times = |start| Times {
            transmit: transmit(),
            pendings: &pendings,
            start: at(start),
            ..Times::default()
        };
        // P2* from the latest pending.
        assert_eq!(judged(false, times(9_000_000)).0, Window::WithinP2Star);
        let timing = judge(LIMITS, false, times(9_000_000));
        assert_eq!(timing.since_latest_pending_ms, Some(5_000.0));
        assert_eq!(
            judged(false, times(9_003_000)),
            (Window::Late, Some(3.0), None)
        );
        // The completion limit from the first, when it is earlier.
        let chain = [at(10_000), at(24_000_000)];
        let times = Times {
            transmit: transmit(),
            pendings: &chain,
            start: at(25_013_000),
            ..Times::default()
        };
        assert_eq!(judged(false, times), (Window::Late, Some(3.0), None));
    }

    #[test]
    fn a_receive_timeout_is_judged_both_ways() {
        let timeout = |us| Times {
            transmit: transmit(),
            timeout: at(us),
            ..Times::default()
        };
        assert_eq!(judged(false, timeout(48_000)).0, Window::TimeoutAtLimit);
        assert_eq!(judged(false, timeout(52_000)).0, Window::TimeoutAtLimit);
        assert_eq!(
            judged(false, timeout(47_000)),
            (Window::TimeoutEarly, Some(3.0), None)
        );
        assert_eq!(
            judged(false, timeout(53_500)),
            (Window::TimeoutLate, Some(3.5), None)
        );
    }

    #[test]
    fn an_end_proves_a_response_on_time_only() {
        let end = |us| Times {
            transmit: transmit(),
            end: at(us),
            ..Times::default()
        };
        let timing = judge(LIMITS, false, end(30_000));
        assert_eq!(timing.window, Window::WithinP2);
        assert_eq!(timing.measured_by, MeasuredBy::VciTxDoneToRxEnd);
        assert_eq!(
            judged(false, end(60_000)),
            (
                Window::NotJudged,
                None,
                Some(NotJudgedReason::EndAfterLimit)
            )
        );
    }

    #[test]
    fn on_an_absorbing_link_unseen_pendings_are_bounded_from_the_transmit() {
        let start = |us| Times {
            transmit: transmit(),
            start: at(us),
            ..Times::default()
        };
        let timeout = |us| Times {
            transmit: transmit(),
            timeout: at(us),
            ..Times::default()
        };
        assert_eq!(judged(true, start(40_000)).0, Window::WithinP2);
        // Past P2: a pending may have come first.
        assert_eq!(judged(true, start(60_000)).0, Window::PossiblePending);
        assert_eq!(judged(true, timeout(60_000)).0, Window::PossiblePending);
        // Early whatever the pendings were, and at P2 within the margin.
        assert_eq!(
            judged(true, timeout(40_000)),
            (Window::TimeoutEarly, Some(10.0), None)
        );
        assert_eq!(judged(true, timeout(50_500)).0, Window::TimeoutAtLimit);
        // Past the bound: late on either history.
        assert_eq!(
            judged(true, start(25_053_000)),
            (Window::Late, Some(3.0), None)
        );
        assert_eq!(
            judged(true, timeout(25_052_500)),
            (Window::TimeoutLate, Some(2.5), None)
        );
        // Without a completion limit there is no bound.
        let unbounded = Limits {
            completion_us: None,
            ..LIMITS
        };
        assert_eq!(
            judge(unbounded, true, timeout(99_000_000)).window,
            Window::PossiblePending
        );
    }

    #[test]
    fn only_times_on_the_transmit_s_clock_and_epoch_are_judged() {
        let other_epoch = Times {
            transmit: transmit(),
            timeout: on(1, 1_050_000),
            ..Times::default()
        };
        assert_eq!(
            judged(false, other_epoch),
            (Window::NotJudged, None, Some(NotJudgedReason::OtherClock))
        );
        let worker = Times {
            transmit: transmit(),
            timeout: Some((
                Clock::Worker,
                Placed {
                    epoch: 0,
                    unwrapped: 1_050_000,
                },
            )),
            ..Times::default()
        };
        assert_eq!(judged(false, worker).2, Some(NotJudgedReason::OtherClock));
        // A pending on another epoch leaves the limit after it unknown.
        let pendings = [on(1, 5)];
        let after = Times {
            transmit: transmit(),
            pendings: &pendings,
            start: at(60_000),
            ..Times::default()
        };
        assert_eq!(judged(false, after).2, Some(NotJudgedReason::OtherClock));
        // Nothing to judge.
        let nothing = Times {
            transmit: transmit(),
            ..Times::default()
        };
        assert_eq!(
            judged(false, nothing).2,
            Some(NotJudgedReason::NoTimeToJudge)
        );
    }
}
