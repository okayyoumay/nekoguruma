//! The judgement of an exchange's times against the limit in force (design 8.11 "Timing
//! judged", "Limit in force", "Possible pending", `timing`).
//!
//! Every time compared is a VCI time placed on its clock (ADR-361), and a difference is taken
//! only between two times on one clock and epoch, from the transmit done. A response start and
//! the VCI's receive timeout are judged both ways; a response end only as proof that the
//! response was on time, since it includes the response's own transfer, except that the end of
//! a message sent as one frame, past the limit by more than that frame's longest transfer,
//! proves it late; a forwarded pending, always one frame, is judged so too (ADR-364). On a link
//! whose VCI
//! absorbs pendings the limit that depends on them is bounded from the transmit: the first
//! pending came within P2, and the 0x78 completion limit runs from it.

use super::clocks::Placed;
use super::exchange::{Clock, MeasuredBy, NotJudgedReason, Timing, Window};
use crate::link::SingleFrame;

/// The length of a response pending (`7F`, the SID, `78`).
const PENDING_BYTES: usize = 3;

/// The limits of a link, as `link_opened` records them, and its timing margin, in
/// microseconds (`None` when no profile gives one, so no time is judged, ADR-377), whether its
/// start-of-message indications are on, and what it sends as one frame (`None` when the log has
/// no bound for the link's protocol).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limits {
    pub p2_us: u64,
    pub p2_star_us: u64,
    pub completion_us: Option<u64>,
    pub margin_us: Option<u64>,
    pub start_indications: bool,
    pub single_frame: Option<SingleFrame>,
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
    /// The length of the response whose end `end` is.
    pub end_len: Option<usize>,
    /// The VCI's receive timeout, when it ended the exchange.
    pub timeout: At,
    /// The ECU may have begun a response: a start of its response was seen, or a start no
    /// exchange could own, or the link has no start-of-message indications to show one.
    pub response_may_have_begun: bool,
}

/// What the times show against the limits, on a link that forwards pendings (`absorbing`
/// false) or absorbs them.
pub(crate) fn judge(limits: Limits, absorbing: bool, times: Times<'_>) -> Timing {
    let mut timing = Timing {
        p2_ms: Some(ms_ceil(limits.p2_us)),
        p2_star_ms: Some(ms_ceil(limits.p2_star_us)),
        completion_ms: limits.completion_us.map(ms_ceil),
        margin_ms: limits.margin_us.map(ms_ceil),
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
    let not_judged = |timing: &mut Timing, reason: NotJudgedReason| {
        timing.window = Window::NotJudged;
        timing.not_judged_reason = Some(reason);
        timing.window_by_ms = None;
    };
    // Without a margin a profile gives, nothing bounds the error of the VCI's times, so no time
    // is compared with a limit (ADR-377); the times stay recorded.
    let Some(margin) = limits.margin_us.map(to_i64) else {
        not_judged(&mut timing, NotJudgedReason::NoMargin);
        return timing;
    };
    let p2 = to_i64(limits.p2_us);
    let p2_star = to_i64(limits.p2_star_us);
    let completion = limits.completion_us.map(to_i64);
    // The longest transfer of a message of `len` bytes the link surely sends as one frame.
    let single_frame = limits.single_frame;
    let transfer = |len: usize| {
        single_frame
            .filter(|frame| len <= frame.max_bytes)
            .map(|frame| to_i64(frame.transfer_us))
    };
    let set = |timing: &mut Timing, window: Window, by_us: Option<i64>| {
        timing.window = window;
        timing.window_by_ms = by_us.map(ms);
        timing.not_judged_reason = None;
    };
    // The pendings seen, in microseconds from the transmit, up to the first that is not on the
    // transmit's clock and epoch: the limit in force is known only up to that one.
    let known: Vec<i64> = times.pendings.iter().map_while(|at| since(*at)).collect();
    let start = since(times.start);
    let end = since(times.end);
    let timeout = since(times.timeout);
    if [start, end, timeout]
        .into_iter()
        .flatten()
        .chain(known.iter().copied())
        .any(|us| us < 0)
    {
        not_judged(&mut timing, NotJudgedReason::BeforeTransmit);
        return timing;
    }
    // Each forwarded pending against the limit in force before it, by its time, which is its
    // end: one past that limit plus the margin and its frame's transfer makes the exchange late,
    // whatever follows (design 8.11 "Limit in force").
    if let Some(pending_transfer) = transfer(PENDING_BYTES) {
        for (place, &at) in known.iter().enumerate() {
            let before = limit_in_force(p2, p2_star, completion, &known[..place], absorbing);
            if let Some(latest) = before.latest
                && at > latest + margin + pending_transfer
            {
                set(
                    &mut timing,
                    Window::Late,
                    Some(at - pending_transfer - latest),
                );
                timing.late_pending = Some(place);
                return timing;
            }
        }
    }
    // A pending not on the transmit's clock and epoch leaves the limit after it unknown.
    if known.len() < times.pendings.len() {
        not_judged(&mut timing, NotJudgedReason::OtherClock);
        return timing;
    }
    let pendings = known;
    let in_force = limit_in_force(p2, p2_star, completion, &pendings, absorbing);
    let within = if pendings.is_empty() {
        Window::WithinP2
    } else {
        Window::WithinP2Star
    };
    // A receive timeout after a response began is the transport's consecutive-frame timer,
    // not the limit in force, so it is judged only when no response may have begun.
    let timeout = timeout.filter(|_| !times.response_may_have_begun);
    if let Some(timeout) = timeout {
        if timeout < in_force.earliest - margin {
            set(
                &mut timing,
                Window::TimeoutEarly,
                Some(in_force.earliest - timeout),
            );
        } else if (timeout - in_force.expected).abs() <= margin {
            set(&mut timing, Window::TimeoutAtLimit, None);
        } else if let Some(latest) = in_force.latest
            && timeout > latest + margin
        {
            set(&mut timing, Window::TimeoutLate, Some(timeout - latest));
        } else {
            set(&mut timing, Window::PossiblePending, None);
        }
    } else if let Some(start) = start {
        if start <= in_force.expected + margin {
            set(&mut timing, within, None);
        } else if let Some(latest) = in_force.latest
            && start > latest + margin
        {
            set(&mut timing, Window::Late, Some(start - latest));
        } else {
            set(&mut timing, Window::PossiblePending, None);
        }
    } else if let Some(end) = end {
        // An end within the limit proves the response on time. One past it proves nothing,
        // since the end includes the response's own transfer, unless the response came as one
        // frame and its end is past the limit by more than that frame's longest transfer.
        if end <= in_force.expected + margin {
            set(&mut timing, within, None);
        } else if let Some(transfer) = times.end_len.and_then(transfer)
            && let Some(latest) = in_force.latest
            && end > latest + margin + transfer
        {
            set(&mut timing, Window::Late, Some(end - transfer - latest));
        } else {
            not_judged(&mut timing, NotJudgedReason::EndAfterLimit);
        }
    } else if times.timeout.is_some() && times.response_may_have_begun {
        not_judged(&mut timing, NotJudgedReason::ResponseBegun);
    } else if times.start.is_some() || times.end.is_some() || times.timeout.is_some() {
        not_judged(&mut timing, NotJudgedReason::OtherClock);
    } else {
        not_judged(&mut timing, NotJudgedReason::NoTimeToJudge);
    }
    timing
}

/// How a response the worker delivered after its exchange ended stands against that exchange's
/// limit in force (design 8.11 `late_response`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LateWindow {
    /// It ended, and so began, before the earliest limit less the margin, this long after the
    /// transmit: the ECU answered in time and the system missed it.
    InTime { at_us: i64 },
    /// It began after the latest limit plus the margin, by at least this much: it came as one
    /// frame, and its end less that frame's longest transfer is past that limit.
    Late { by_us: i64 },
    /// Neither is shown: its time is between, it may span frames, it cannot be judged on one
    /// clock with the transmit, or no profile gives the link's margin (ADR-377).
    Open,
}

/// Judges a late response's time `at`, which is its end, of `len` bytes, against the limit in
/// force of the exchange whose transmit was `transmit` and whose pendings were `pendings`.
pub(crate) fn judge_late(
    limits: Limits,
    absorbing: bool,
    transmit: At,
    pendings: &[At],
    at: At,
    len: usize,
) -> LateWindow {
    // Only a response that fits one frame is shown late by its end: less that frame's longest
    // transfer, it began past the limit.
    let transfer = limits
        .single_frame
        .filter(|frame| len <= frame.max_bytes)
        .map(|frame| to_i64(frame.transfer_us));
    judge_late_at(limits, absorbing, transmit, pendings, at, transfer)
}

/// Judges the time `at` a late start-of-message indication shows its response beginning,
/// against the limit in force as [`judge_late`] judges a response's end (design 8.11
/// "Settlement" `late_item`): a start is the beginning itself, so no transfer comes off it.
pub(crate) fn judge_late_start(
    limits: Limits,
    absorbing: bool,
    transmit: At,
    pendings: &[At],
    at: At,
) -> LateWindow {
    judge_late_at(limits, absorbing, transmit, pendings, at, Some(0))
}

/// [`judge_late`] for a time `at` that began its response `transfer` before it, when that is
/// bounded.
fn judge_late_at(
    limits: Limits,
    absorbing: bool,
    transmit: At,
    pendings: &[At],
    at: At,
    transfer: Option<i64>,
) -> LateWindow {
    let (Some((clock, transmit)), Some((own, at))) = (transmit, at) else {
        return LateWindow::Open;
    };
    if own != clock || at.epoch != transmit.epoch {
        return LateWindow::Open;
    }
    let Some(pendings) = pendings
        .iter()
        .map(|pending| {
            let (own, pending) = (*pending)?;
            (own == clock && pending.epoch == transmit.epoch)
                .then_some(pending.unwrapped - transmit.unwrapped)
        })
        .collect::<Option<Vec<i64>>>()
    else {
        return LateWindow::Open;
    };
    let at = at.unwrapped - transmit.unwrapped;
    // A final response comes after the transmit and after every pending of its exchange.
    if at < 0 || pendings.iter().any(|&us| us < 0 || us > at) {
        return LateWindow::Open;
    }
    let Some(margin) = limits.margin_us.map(to_i64) else {
        return LateWindow::Open;
    };
    let in_force = limit_in_force(
        to_i64(limits.p2_us),
        to_i64(limits.p2_star_us),
        limits.completion_us.map(to_i64),
        &pendings,
        absorbing,
    );
    if at <= in_force.earliest - margin {
        return LateWindow::InTime { at_us: at };
    }
    match (transfer, in_force.latest) {
        (Some(transfer), Some(latest)) if at - transfer > latest + margin => LateWindow::Late {
            by_us: at - transfer - latest,
        },
        _ => LateWindow::Open,
    }
}

/// How far the VCI's device clock, read at the agent's deadline, was past the VCI's own limits
/// (design 8.11 `vci_did_not_end`): the reading less its round trip, from the transmit, past
/// the latest limit in force plus the margin plus the longest transfer of a single-frame
/// response, which would have given no start-of-message indication. `None` when it is not past,
/// or cannot be shown: no profile gives the link's margin (ADR-377), the reading or the transmit is not on the device clock (on a J2534 link
/// the reading is the worker's, ADR-120), they are on different epochs, a pending is not on
/// their clock and epoch or came before the transmit, nothing bounds the limit, or the link's
/// protocol has no single-frame bound.
pub(crate) fn past_vci_limits(
    limits: Limits,
    absorbing: bool,
    transmit: At,
    pendings: &[At],
    reading: At,
    round_trip_us: u64,
) -> Option<i64> {
    let (Clock::Device, transmit) = transmit? else {
        return None;
    };
    let (Clock::Device, reading) = reading? else {
        return None;
    };
    if reading.epoch != transmit.epoch {
        return None;
    }
    let pendings = pendings
        .iter()
        .map(|at| {
            let (own, at) = (*at)?;
            (own == Clock::Device && at.epoch == transmit.epoch)
                .then_some(at.unwrapped - transmit.unwrapped)
        })
        .collect::<Option<Vec<i64>>>()?;
    if pendings.iter().any(|&us| us < 0) {
        return None;
    }
    let transfer = to_i64(limits.single_frame?.transfer_us);
    let latest = limit_in_force(
        to_i64(limits.p2_us),
        to_i64(limits.p2_star_us),
        limits.completion_us.map(to_i64),
        &pendings,
        absorbing,
    )
    .latest?;
    let elapsed = reading.unwrapped - to_i64(round_trip_us) - transmit.unwrapped;
    let by = elapsed - (latest + to_i64(limits.margin_us?) + transfer);
    (by > 0).then_some(by)
}

/// The limit in force, in microseconds from the transmit (design 8.11 "Limit in force").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InForce {
    /// The limit the pendings seen give: P2 until a pending, then the earlier of P2* from the
    /// latest and the completion limit from the first.
    expected: i64,
    /// The earliest the limit can be, whatever pendings the VCI absorbed unseen.
    earliest: i64,
    /// The latest it can be; `None` when nothing bounds it (no completion limit, on a link
    /// whose VCI absorbs pendings).
    latest: Option<i64>,
}

fn limit_in_force(
    p2: i64,
    p2_star: i64,
    completion: Option<i64>,
    pendings: &[i64],
    absorbing: bool,
) -> InForce {
    let after = |first: i64, latest: i64| {
        let star = latest + p2_star;
        completion.map_or(star, |completion| star.min(first + completion))
    };
    let expected = match (pendings.first(), pendings.last()) {
        (Some(first), Some(latest)) => after(*first, *latest),
        _ => p2,
    };
    if !absorbing {
        return InForce {
            expected,
            earliest: expected,
            latest: Some(expected),
        };
    }
    // Unseen pendings: the first, seen or not, came at or after the transmit and within P2,
    // and the latest at or after the latest seen; the completion limit runs from the first.
    let earliest = match pendings.last() {
        // An unseen pending at the transmit gives the earliest end of a chain.
        None => p2.min(after(0, 0)),
        Some(latest) => after(0, *latest),
    };
    let latest = completion.map(|completion| pendings.first().copied().unwrap_or(p2) + completion);
    InForce {
        expected,
        earliest: earliest.min(expected),
        latest: latest.map(|latest| latest.max(expected)),
    }
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
        margin_us: Some(2_000),
        start_indications: true,
        single_frame: Some(SingleFrame {
            max_bytes: 6,
            transfer_us: 320,
        }),
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
    fn with_no_margin_a_profile_gives_no_time_is_judged() {
        // ADR-377: the agent has no default margin, so nothing bounds the error of the VCI's
        // times; they are recorded with the limits, and no comparison is made, however far
        // past a limit they are.
        let limits = Limits {
            margin_us: None,
            ..LIMITS
        };
        let times = Times {
            transmit: transmit(),
            start: at(900_000),
            ..Times::default()
        };
        assert_eq!(judged(false, times).0, Window::Late);
        let timing = judge(limits, false, times);
        assert_eq!(timing.window, Window::NotJudged);
        assert_eq!(timing.not_judged_reason, Some(NotJudgedReason::NoMargin));
        assert_eq!(timing.response_ms, Some(900.0));
        assert_eq!((timing.p2_ms, timing.margin_ms), (Some(50), None));
        let timeout = Times {
            transmit: transmit(),
            timeout: at(50_000),
            ..Times::default()
        };
        assert_eq!(
            judge(limits, false, timeout).not_judged_reason,
            Some(NotJudgedReason::NoMargin)
        );
        // A late response and a reading at the deadline are not judged either.
        assert_eq!(
            judge_late(limits, false, transmit(), &[], at(900_000), 4),
            LateWindow::Open
        );
        assert_ne!(
            judge_late(LIMITS, false, transmit(), &[], at(900_000), 4),
            LateWindow::Open
        );
        assert_eq!(
            past_vci_limits(limits, false, transmit(), &[], at(900_000), 0),
            None
        );
        assert!(past_vci_limits(LIMITS, false, transmit(), &[], at(900_000), 0).is_some());
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
        let chain = [
            at(10_000),
            at(5_000_000),
            at(10_000_000),
            at(15_000_000),
            at(20_000_000),
            at(24_000_000),
        ];
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

    /// An end `us` after the transmit of a response of `len` bytes.
    fn end_of(len: usize, us: i64) -> Times<'static> {
        Times {
            transmit: transmit(),
            end: at(us),
            end_len: Some(len),
            ..Times::default()
        }
    }

    #[test]
    fn a_single_frame_end_past_the_limit_and_its_transfer_is_late() {
        // P2 50 ms, the margin 2 ms and one frame's 320 µs.
        assert_eq!(judged(false, end_of(6, 30_000)).0, Window::WithinP2);
        assert_eq!(
            judged(false, end_of(6, 52_320)),
            (
                Window::NotJudged,
                None,
                Some(NotJudgedReason::EndAfterLimit)
            )
        );
        // Late by how much its start was at least past P2.
        assert_eq!(
            judged(false, end_of(3, 52_321)),
            (Window::Late, Some(2.001), None)
        );
        assert_eq!(judge(LIMITS, false, end_of(3, 52_321)).late_pending, None);
        // Six bytes are still one frame.
        assert_eq!(judged(false, end_of(6, 52_321)).0, Window::Late);
    }

    #[test]
    fn a_multi_frame_end_is_never_late() {
        assert_eq!(
            judged(false, end_of(7, 9_000_000)),
            (
                Window::NotJudged,
                None,
                Some(NotJudgedReason::EndAfterLimit)
            )
        );
        // Nor an end whose length or link gives no frame to bound.
        assert_eq!(
            judged(
                false,
                Times {
                    end_len: None,
                    ..end_of(3, 9_000_000)
                }
            )
            .2,
            Some(NotJudgedReason::EndAfterLimit)
        );
        let unbounded = Limits {
            single_frame: None,
            ..LIMITS
        };
        assert_eq!(
            judge(unbounded, false, end_of(3, 9_000_000)).not_judged_reason,
            Some(NotJudgedReason::EndAfterLimit)
        );
    }

    #[test]
    fn a_forwarded_pending_is_judged_against_the_limit_before_it() {
        let times = |pendings| Times {
            transmit: transmit(),
            pendings,
            start: at(5_000_000),
            ..Times::default()
        };
        // The first against P2, by its end less its frame's transfer.
        let on_time = [at(52_320)];
        assert_eq!(judged(false, times(&on_time)).0, Window::WithinP2Star);
        let late = [at(52_321)];
        let timing = judge(LIMITS, false, times(&late));
        assert_eq!(
            (timing.window, timing.window_by_ms, timing.late_pending),
            (Window::Late, Some(2.001), Some(0))
        );
        // A later one against P2* from the one before it.
        let second = [at(10_000), at(5_012_321)];
        let timing = judge(LIMITS, false, times(&second));
        assert_eq!(
            (timing.window, timing.window_by_ms, timing.late_pending),
            (Window::Late, Some(2.001), Some(1))
        );
        // Whatever follows: a receive timeout at the limit after it.
        let timing = judge(
            LIMITS,
            false,
            Times {
                transmit: transmit(),
                pendings: &late,
                timeout: at(5_052_321),
                ..Times::default()
            },
        );
        assert_eq!(
            (timing.window, timing.late_pending),
            (Window::Late, Some(0))
        );
        // The first late pending is the one named.
        let both = [at(52_321), at(5_060_000)];
        assert_eq!(judge(LIMITS, false, times(&both)).late_pending, Some(0));
        // One on another clock after a late one leaves the late one judged, and the limits
        // after it unknown.
        let then_other = [at(52_321), on(1, 5)];
        assert_eq!(
            judge(LIMITS, false, times(&then_other)).late_pending,
            Some(0)
        );
        let on_time_then_other = [at(10_000), on(1, 5)];
        assert_eq!(
            judged(false, times(&on_time_then_other)).2,
            Some(NotJudgedReason::OtherClock)
        );
        // A pending before the transmit judges nothing.
        let before = [at(-10_000), at(4_995_000)];
        assert_eq!(
            judged(false, times(&before)).2,
            Some(NotJudgedReason::BeforeTransmit)
        );
        // No bound for the link's frames: the pendings are not judged.
        let unbounded = Limits {
            single_frame: None,
            ..LIMITS
        };
        assert_eq!(
            judge(unbounded, false, times(&late)).window,
            Window::WithinP2Star
        );
    }

    #[test]
    fn on_an_absorbing_link_a_pending_is_late_only_past_the_latest_limit() {
        let times = |pendings| Times {
            transmit: transmit(),
            pendings,
            start: at(30_000_000),
            ..Times::default()
        };
        // Unseen pendings may have come before it: the latest limit is the completion limit
        // from P2.
        let passed_on = [at(25_052_320)];
        assert_eq!(judge(LIMITS, true, times(&passed_on)).late_pending, None);
        let late = [at(25_052_321)];
        let timing = judge(LIMITS, true, times(&late));
        assert_eq!(
            (timing.window, timing.window_by_ms, timing.late_pending),
            (Window::Late, Some(2.001), Some(0))
        );
        // With no completion limit nothing bounds the limit before it.
        let unbounded = Limits {
            completion_us: None,
            ..LIMITS
        };
        assert_eq!(judge(unbounded, true, times(&late)).late_pending, None);
    }

    #[test]
    fn a_late_response_is_in_time_late_or_open() {
        let late = |limits, absorbing, pendings: &[At], at_us, len| {
            judge_late(limits, absorbing, transmit(), pendings, at(at_us), len)
        };
        // P2 50 ms, the margin 2 ms, one frame 320 µs.
        assert_eq!(
            late(LIMITS, false, &[], 48_000, 3),
            LateWindow::InTime { at_us: 48_000 }
        );
        assert_eq!(late(LIMITS, false, &[], 48_001, 3), LateWindow::Open);
        assert_eq!(late(LIMITS, false, &[], 52_320, 3), LateWindow::Open);
        assert_eq!(
            late(LIMITS, false, &[], 52_321, 3),
            LateWindow::Late { by_us: 2_001 }
        );
        // A response that may span frames is never shown late by its end.
        assert_eq!(late(LIMITS, false, &[], 9_000_000, 7), LateWindow::Open);
        // After a pending, against the limit it sets.
        assert_eq!(
            late(LIMITS, false, &[at(10_000)], 4_000_000, 3),
            LateWindow::InTime { at_us: 4_000_000 }
        );
        // On an absorbing link, in time only before the earliest limit and late only past the
        // latest.
        assert_eq!(
            late(LIMITS, true, &[], 48_000, 3),
            LateWindow::InTime { at_us: 48_000 }
        );
        assert_eq!(late(LIMITS, true, &[], 9_000_000, 3), LateWindow::Open);
        assert_eq!(
            late(LIMITS, true, &[], 25_052_321, 3),
            LateWindow::Late { by_us: 2_001 }
        );
        // Not on one clock and epoch with the transmit, or before it.
        assert_eq!(
            judge_late(LIMITS, false, transmit(), &[], on(1, 5), 3),
            LateWindow::Open
        );
        assert_eq!(
            late(LIMITS, false, &[on(1, 5)], 9_000_000, 3),
            LateWindow::Open
        );
        assert_eq!(late(LIMITS, false, &[], -1, 3), LateWindow::Open);
        // A start is its response's beginning: late as soon as it is past the limit plus the
        // margin, with no frame's transfer off it, and on a link with no single-frame bound too.
        let start = |limits, at_us| judge_late_start(limits, false, transmit(), &[], at(at_us));
        assert_eq!(start(LIMITS, 52_000), LateWindow::Open);
        assert_eq!(start(LIMITS, 52_001), LateWindow::Late { by_us: 2_001 });
        let unbounded = Limits {
            single_frame: None,
            ..LIMITS
        };
        assert_eq!(start(unbounded, 52_001), LateWindow::Late { by_us: 2_001 });
        assert_eq!(start(LIMITS, 48_000), LateWindow::InTime { at_us: 48_000 });
        // Before a pending of the exchange: no final response of it.
        assert_eq!(
            late(LIMITS, false, &[at(30_000)], 20_000, 3),
            LateWindow::Open
        );
    }

    #[test]
    fn the_vci_s_own_limits_are_past_by_the_reading_less_its_round_trip() {
        let past = |limits, absorbing, pendings: &[At], reading: At, round_trip_us| {
            past_vci_limits(
                limits,
                absorbing,
                transmit(),
                pendings,
                reading,
                round_trip_us,
            )
        };
        // P2 50 ms, the margin 2 ms and one frame's 320 µs: past by 1 µs, less a 1 ms round
        // trip.
        assert_eq!(past(LIMITS, false, &[], at(53_321), 1_000), Some(1));
        assert_eq!(past(LIMITS, false, &[], at(53_320), 1_000), None);
        // After a pending, from the limit it sets.
        assert_eq!(
            past(LIMITS, false, &[at(10_000)], at(5_012_321), 0),
            Some(1)
        );
        // A pending not on the clock, or before the transmit, leaves the limit unknown.
        assert_eq!(past(LIMITS, false, &[on(1, 5)], at(9_000_000), 0), None);
        assert_eq!(past(LIMITS, false, &[at(-1)], at(9_000_000), 0), None);
        // A reading on another epoch, or on the worker's clock (J2534, ADR-120).
        assert_eq!(past(LIMITS, false, &[], on(1, 9_000_000), 0), None);
        let worker = Some((
            Clock::Worker,
            Placed {
                epoch: 0,
                unwrapped: 10_000_000,
            },
        ));
        assert_eq!(past(LIMITS, false, &[], worker, 0), None);
        // On an absorbing link, past the latest limit only: P2 plus the completion limit.
        assert_eq!(past(LIMITS, true, &[], at(1_000_000), 0), None);
        assert_eq!(past(LIMITS, true, &[], at(25_052_321), 0), Some(1));
        let unbounded = Limits {
            completion_us: None,
            ..LIMITS
        };
        assert_eq!(past(unbounded, true, &[], at(99_000_000), 0), None);
        // No single-frame bound for the link's protocol.
        let no_frame = Limits {
            single_frame: None,
            ..LIMITS
        };
        assert_eq!(past(no_frame, false, &[], at(9_000_000), 0), None);
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

    /// A receive timeout after the ECU may have begun a response is the transport's
    /// consecutive-frame timer, not the limit in force: the start is judged instead, and with
    /// no start on the clock nothing is.
    #[test]
    fn a_timeout_after_a_response_began_is_not_judged_against_p2() {
        let begun = |start: At| Times {
            transmit: transmit(),
            start,
            timeout: at(1_020_000),
            response_may_have_begun: true,
            ..Times::default()
        };
        assert_eq!(judged(false, begun(at(20_000))).0, Window::WithinP2);
        assert_eq!(
            judged(false, begun(None)),
            (
                Window::NotJudged,
                None,
                Some(NotJudgedReason::ResponseBegun)
            )
        );
    }

    /// On a link whose VCI absorbs pendings, a pending the worker still passed on restarts the
    /// limit: a timeout at P2 after it is early, whatever unseen pendings there were.
    #[test]
    fn a_pending_seen_on_an_absorbing_link_counts() {
        let pendings = [at(30_000)];
        let timeout = |us| Times {
            transmit: transmit(),
            pendings: &pendings,
            timeout: at(us),
            ..Times::default()
        };
        assert_eq!(
            judged(true, timeout(50_500)),
            (Window::TimeoutEarly, Some(4_979.5), None)
        );
        assert_eq!(judged(true, timeout(5_030_000)).0, Window::TimeoutAtLimit);
        // Past P2* from the seen pending, an unseen later one may have restarted it.
        assert_eq!(judged(true, timeout(6_000_000)).0, Window::PossiblePending);
        // Past the completion limit from the seen first: late on any history.
        assert_eq!(
            judged(true, timeout(25_033_000)),
            (Window::TimeoutLate, Some(3.0), None)
        );
        let start = Times {
            transmit: transmit(),
            pendings: &pendings,
            start: at(4_000_000),
            ..Times::default()
        };
        assert_eq!(judged(true, start).0, Window::WithinP2Star);
    }

    /// With a completion limit below P2, an unseen pending can end the chain before P2, so
    /// only a timeout before that is early on an absorbing link.
    #[test]
    fn the_earliest_limit_on_an_absorbing_link_is_the_shortest() {
        let short = Limits {
            completion_us: Some(5_000),
            ..LIMITS
        };
        let timeout = |us| Times {
            transmit: transmit(),
            timeout: at(us),
            ..Times::default()
        };
        assert_eq!(
            judge(short, true, timeout(15_000)).window,
            Window::PossiblePending
        );
        assert_eq!(
            judge(short, true, timeout(2_000)).window,
            Window::TimeoutEarly
        );
    }

    #[test]
    fn a_time_before_the_transmit_is_not_judged() {
        let before = Times {
            transmit: transmit(),
            start: at(-500),
            end: at(3_000),
            ..Times::default()
        };
        assert_eq!(
            judged(false, before),
            (
                Window::NotJudged,
                None,
                Some(NotJudgedReason::BeforeTransmit)
            )
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
