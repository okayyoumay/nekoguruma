//! The VCI's clocks as the communication log follows them (design 8.11 "Timing judged",
//! `clock_epoch`, ADR-361): each clock's 32-bit microsecond times aligned with the agent's
//! monotonic clock and unwrapped, and a new epoch opened where the time base did not carry on.
//!
//! A clock's alignment is the least delay seen in its epoch between a VCI time and the agent
//! reading the item that carried it. A raw time is unwrapped to the first value at or above the
//! clock's latest time less the clock margin: a clock does not run back. That value is checked
//! against the most the clock can have run since: the agent's read time less that least delay,
//! plus the drift bound over the agent time since the least delay was seen and the clock margin
//! (design 8.11 "Drift bound"). A value past a wrap that the clock cannot have reached is a step
//! back that no wrap explains, or a run the tolerances do not cover, and opens a new epoch. The
//! tolerances tell a wrap from a step back, never how many wraps a silence holds: once the agent
//! has read nothing on the clock for half a wrap of its own time, the next time opens a new epoch
//! whatever its value, since the model of a clock this log follows (a microsecond counter running
//! at less than twice the agent's rate) no longer decides the wrap count (ADR-383). So does the
//! first time read after the VCI reconnected. A value ahead of the bound without a wrap is kept:
//! it shows only that the reads before it came later than their times, since the delay of a read
//! has no ceiling.

use serde::{Deserialize, Serialize};

use super::exchange::Clock;

/// The span of the VCI's 32-bit microsecond times (ISO 22900-2:2022 8.1.6.1).
const WRAP: i128 = 1 << 32;

/// Half the span, on the agent's clock: after this long with nothing read on a clock, how many
/// wraps it has run is open for a clock at up to twice the agent's rate (ADR-383).
const HALF_WRAP_US: u64 = 1 << 31;

/// Why a clock's epoch began (`clock_epoch`'s `reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EpochReason {
    /// The clock's first time in this run of the log.
    First,
    /// The time lies behind the clock's latest one, and the tolerances do not let the clock have
    /// run to the next wrap since: a step back, or a run faster than they cover.
    BackwardStep,
    /// The agent read nothing on the clock for half a wrap or more, so how many wraps passed is
    /// unknown (ADR-383).
    GapBeyondWrap,
    /// The first time read after the VCI connected again.
    Reconnect,
}

/// A time placed on its clock: the epoch, and its value unwrapped within the epoch (the
/// epoch's first raw time plus the microseconds since; below it for a time an item carries from
/// before the epoch's first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placed {
    pub epoch: u32,
    pub unwrapped: i64,
}

/// One time of a `clock_epoch` event: raw, with the agent's monotonic time when the host read
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct EpochTime {
    pub us: u64,
    pub read_us: u64,
}

/// The time an epoch ended with: raw, with its epoch and when the host read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EndedEpoch {
    pub epoch: u32,
    pub us: u64,
    pub read_us: u64,
}

/// A `clock_epoch` event: the clock, its new epoch and why, the time that began it and the
/// latest time of the epoch before it, when there was one in this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct ClockEpoch {
    pub clock: Clock,
    pub epoch: u32,
    pub reason: EpochReason,
    pub agent_epoch: u32,
    pub first: EpochTime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<EndedEpoch>,
}

/// The tolerances the clocks are followed with, from `vci_connected` (ADR-357).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tolerance {
    pub drift_bound_ppm: u32,
    pub clock_margin_us: u32,
    /// When the exchange whose times are placed began, on the agent's clock: its primitive's
    /// own items were taken after it, so the clock can have run since its latest time at least
    /// as long as the agent has since then.
    pub began_us: Option<u64>,
}

impl Tolerance {
    /// The tolerance over `elapsed_us` of the agent's clock.
    fn over(self, elapsed_us: u64) -> i128 {
        i128::from(self.drift_bound_ppm) * i128::from(elapsed_us) / 1_000_000
            + i128::from(self.clock_margin_us)
    }
}

/// A time the host read: raw, unwrapped, and when it was read.
#[derive(Debug, Clone, Copy)]
struct Reading {
    raw: u64,
    unwrapped: i128,
    read_us: u64,
}

#[derive(Debug, Clone, Copy)]
struct Epoch {
    number: u32,
    latest: Reading,
    /// The least delay seen (agent time less unwrapped time) and when it was seen.
    least_delay: i128,
    least_at_us: u64,
    /// The agent's latest read of a time kept in the epoch, from which a silence is measured.
    last_read_us: u64,
}

impl Epoch {
    /// The agent has read nothing on the clock for half a wrap by `at_us`, so how many wraps
    /// the clock has run since is open (ADR-383).
    fn silent_at(&self, at_us: u64) -> bool {
        at_us.saturating_sub(self.last_read_us) >= HALF_WRAP_US
    }
}

#[derive(Debug, Default)]
struct ClockState {
    current: Option<Epoch>,
    /// The VCI connected again since the latest time.
    reconnected: bool,
}

/// The state of the VCI's clocks for one run of the log.
#[derive(Debug, Default)]
pub(crate) struct VciClocks {
    device: ClockState,
    worker: ClockState,
    /// The epoch each clock opens next: `device`, then `worker`.
    next: [u32; 2],
}

impl VciClocks {
    /// The clocks of a run whose log already holds epochs below `next` (`device`, `worker`), so
    /// an epoch number is never given twice in one log.
    pub(crate) fn starting_at(next: [u32; 2]) -> Self {
        Self {
            next,
            ..Self::default()
        }
    }

    fn state(&mut self, clock: Clock) -> &mut ClockState {
        match clock {
            Clock::Device => &mut self.device,
            Clock::Worker => &mut self.worker,
        }
    }

    /// The VCI connected again: the next time of each clock opens a new epoch, since nothing
    /// yet shows that its time base carried on (design 8.11 `vci_loss_settled`).
    pub(crate) fn reconnect(&mut self) {
        for state in [&mut self.device, &mut self.worker] {
            state.reconnected = state.current.is_some();
        }
    }

    /// Places `raw`, a time on `clock` the host read at `read_us` of agent epoch `agent_epoch`,
    /// and takes it into the clock's state. The `clock_epoch` event when it opened an epoch.
    pub(crate) fn read(
        &mut self,
        clock: Clock,
        raw: u64,
        read_us: u64,
        agent_epoch: u32,
        tolerance: Tolerance,
    ) -> (Placed, Option<ClockEpoch>) {
        let index = match clock {
            Clock::Device => 0,
            Clock::Worker => 1,
        };
        let next = self.next[index];
        let state = self.state(clock);
        let reading = |unwrapped: i128| Reading {
            raw,
            unwrapped,
            read_us,
        };
        let reason = match (state.current, state.reconnected) {
            (None, _) => EpochReason::First,
            (Some(_), true) => EpochReason::Reconnect,
            (Some(epoch), false) if epoch.silent_at(read_us) => EpochReason::GapBeyondWrap,
            (Some(mut epoch), false) => {
                let raw = i128::from(raw);
                let mut reach = i128::from(read_us) - epoch.least_delay
                    + tolerance.over(read_us.saturating_sub(epoch.least_at_us));
                // Times read together, or soon after a late read, can lie past that reach by
                // the read's delay; the agent's time since the exchange began bounds the run
                // since the latest time, if that was taken in the exchange.
                if let Some(began_us) = tolerance.began_us {
                    let since = read_us.saturating_sub(began_us);
                    reach = reach
                        .max(epoch.latest.unwrapped + i128::from(since) + tolerance.over(since));
                }
                let lower = epoch.latest.unwrapped - i128::from(tolerance.clock_margin_us);
                // The first value at or above `lower`, and the wraps it lies past the latest.
                let unwrapped = raw - (raw - lower).div_euclid(WRAP) * WRAP;
                let wrapped =
                    (unwrapped - raw) > (epoch.latest.unwrapped - i128::from(epoch.latest.raw));
                if wrapped && unwrapped > reach {
                    EpochReason::BackwardStep
                } else {
                    if unwrapped > epoch.latest.unwrapped {
                        epoch.latest = reading(unwrapped);
                    }
                    epoch.last_read_us = epoch.last_read_us.max(read_us);
                    let delay = i128::from(read_us) - unwrapped;
                    if delay < epoch.least_delay {
                        epoch.least_delay = delay;
                        epoch.least_at_us = read_us;
                    }
                    state.current = Some(epoch);
                    return (
                        Placed {
                            epoch: epoch.number,
                            unwrapped: i64::try_from(unwrapped).unwrap_or(i64::MAX),
                        },
                        None,
                    );
                }
            }
        };
        let previous = state.current.map(|epoch| EndedEpoch {
            epoch: epoch.number,
            us: epoch.latest.raw,
            read_us: epoch.latest.read_us,
        });
        let unwrapped = i128::from(raw);
        state.current = Some(Epoch {
            number: next,
            latest: reading(unwrapped),
            least_delay: i128::from(read_us) - unwrapped,
            least_at_us: read_us,
            last_read_us: read_us,
        });
        state.reconnected = false;
        self.next[index] = next.saturating_add(1);
        (
            Placed {
                epoch: next,
                unwrapped: i64::try_from(raw).unwrap_or(i64::MAX),
            },
            Some(ClockEpoch {
                clock,
                epoch: next,
                reason,
                agent_epoch,
                first: EpochTime { us: raw, read_us },
                previous,
            }),
        )
    }

    /// The clock's latest time in its current epoch, raw, with when the host read it: what a
    /// lost VCI's record keeps of the clock (design 8.11 `vci_loss_settled`, ADR-373).
    pub(crate) fn last(&self, clock: Clock) -> Option<EndedEpoch> {
        let state = match clock {
            Clock::Device => &self.device,
            Clock::Worker => &self.worker,
        };
        let epoch = state.current?;
        Some(EndedEpoch {
            epoch: epoch.number,
            us: epoch.latest.raw,
            read_us: epoch.latest.read_us,
        })
    }

    /// The clock's latest unwrapped time in its current epoch, for a time read at `at_us`;
    /// `None` when the agent had read nothing on the clock for half a wrap by then (ADR-383).
    pub(crate) fn latest(&self, clock: Clock, at_us: u64) -> Option<i64> {
        let state = match clock {
            Clock::Device => &self.device,
            Clock::Worker => &self.worker,
        };
        let epoch = state.current.filter(|epoch| !epoch.silent_at(at_us))?;
        let latest = epoch.latest.unwrapped;
        i64::try_from(latest).ok()
    }

    /// Places `raw`, a time on `clock` with no read time of its own that the host read by
    /// `by_us`, nearest the clock's latest time, without taking it into the clock's state.
    /// `None` when the clock has no epoch, the VCI connected again since its latest time, or the
    /// agent had read nothing on the clock for half a wrap by `by_us` (ADR-383).
    pub(crate) fn near_latest(&self, clock: Clock, raw: u64, by_us: u64) -> Option<Placed> {
        let state = match clock {
            Clock::Device => &self.device,
            Clock::Worker => &self.worker,
        };
        let epoch = state
            .current
            .filter(|epoch| !state.reconnected && !epoch.silent_at(by_us))?;
        let raw = i128::from(raw);
        let k = (epoch.latest.unwrapped - raw + WRAP / 2).div_euclid(WRAP);
        Some(Placed {
            epoch: epoch.number,
            unwrapped: i64::try_from(raw + k * WRAP).ok()?,
        })
    }
}

/// How the first reading of a lost VCI's clock after its reconnect compares with the gap the
/// agent's clock measured since the clock's last time before the loss (design 8.11
/// `vci_loss_settled`, ADR-373).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LossFit {
    /// What a clock that carried on reads: the last time plus the gap, modulo the wrap.
    pub expected_us: u64,
    /// Twice the clock margin, one for each reading, plus the reading's round trip, within
    /// which the clock was read, plus the drift bound over the gap.
    pub tolerance_us: u64,
    /// The reading lies within the tolerance of the expected value, across the wrap.
    pub carried_on: bool,
    /// The reading is no more than the gap plus the tolerance: a clock that started again from
    /// zero during the gap.
    pub restarted: bool,
}

/// Sets `reading_us` against `last_us` and the `gap_us` between their reads (raw times of one
/// clock; the gap on one clock of the agent's machine), where the reading was taken within
/// `round_trip_us` before its read.
pub(crate) fn loss_fit(
    last_us: u64,
    reading_us: u64,
    gap_us: u64,
    round_trip_us: u64,
    drift_bound_ppm: u32,
    clock_margin_us: u32,
) -> LossFit {
    let tolerance = 2 * i128::from(clock_margin_us)
        + i128::from(round_trip_us)
        + i128::from(drift_bound_ppm) * i128::from(gap_us) / 1_000_000;
    let reading = i128::from(reading_us);
    let expected = (i128::from(last_us) + i128::from(gap_us)).rem_euclid(WRAP);
    let ahead = (reading - expected).rem_euclid(WRAP);
    let distance = ahead.min(WRAP - ahead);
    LossFit {
        expected_us: u64::try_from(expected).unwrap_or(u64::MAX),
        tolerance_us: u64::try_from(tolerance).unwrap_or(u64::MAX),
        carried_on: distance <= tolerance,
        restarted: reading <= i128::from(gap_us) + tolerance,
    }
}

/// The unwrapped value of `raw` nearest `anchor`.
pub(crate) fn nearest(anchor: i64, raw: u64) -> i64 {
    let raw = i128::from(raw);
    let k = (i128::from(anchor) - raw + WRAP / 2).div_euclid(WRAP);
    i64::try_from(raw + k * WRAP).unwrap_or(i64::MAX)
}

/// Places `raw`, a time an item carries with a later one of its own (`anchor`, on the same
/// clock), at or before the anchor: a response's transmit done and start come before its end.
pub(crate) fn before(anchor: Placed, raw: u64) -> Placed {
    let anchor_us = i128::from(anchor.unwrapped);
    let raw = i128::from(raw);
    let k = (anchor_us - raw).div_euclid(WRAP);
    let unwrapped = raw + k * WRAP;
    Placed {
        epoch: anchor.epoch,
        unwrapped: i64::try_from(unwrapped).unwrap_or(i64::MIN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOLERANCE: Tolerance = Tolerance {
        drift_bound_ppm: 100,
        clock_margin_us: 1_000,
        began_us: None,
    };
    const SPAN: u64 = 1 << 32;

    /// Reads `raw` on the device clock at `read_us` of agent epoch 0.
    fn read(clocks: &mut VciClocks, raw: u64, read_us: u64) -> (Placed, Option<ClockEpoch>) {
        clocks.read(Clock::Device, raw, read_us, 0, TOLERANCE)
    }

    fn placed(epoch: u32, unwrapped: i64) -> Placed {
        Placed { epoch, unwrapped }
    }

    #[test]
    fn the_first_time_opens_epoch_zero() {
        let mut clocks = VciClocks::default();
        let (at, opened) = read(&mut clocks, 5_000, 7_000);
        assert_eq!(at, placed(0, 5_000));
        assert_eq!(
            opened,
            Some(ClockEpoch {
                clock: Clock::Device,
                epoch: 0,
                reason: EpochReason::First,
                agent_epoch: 0,
                first: EpochTime {
                    us: 5_000,
                    read_us: 7_000
                },
                previous: None,
            })
        );
        // Each clock has its own epochs.
        let (worker, opened) = clocks.read(Clock::Worker, 9, 7_100, 0, TOLERANCE);
        assert_eq!(worker, placed(0, 9));
        assert_eq!(opened.map(|opened| opened.reason), Some(EpochReason::First));
    }

    #[test]
    fn a_wrap_is_unwrapped_within_the_epoch() {
        let mut clocks = VciClocks::default();
        // 10 ms before the wrap, read 2 ms later.
        read(&mut clocks, SPAN - 10_000, 2_000);
        // 30 ms later the raw time has wrapped to 20 ms.
        let (at, opened) = read(&mut clocks, 20_000, 32_000);
        assert_eq!(opened, None);
        assert_eq!(at, placed(0, (SPAN + 20_000) as i64));
        // And the epoch goes on past it.
        let (at, opened) = read(&mut clocks, 25_000, 40_000);
        assert_eq!((at, opened), (placed(0, (SPAN + 25_000) as i64), None));
    }

    #[test]
    fn a_time_read_late_stays_in_the_epoch() {
        let mut clocks = VciClocks::default();
        read(&mut clocks, 100_000, 101_000);
        // Read 400 ms after the VCI took it: a slow event path, not a step.
        let (at, opened) = read(&mut clocks, 150_000, 551_000);
        assert_eq!((at, opened), (placed(0, 150_000), None));
        // Within the clock margin behind the latest time: the same epoch.
        let (at, opened) = read(&mut clocks, 149_500, 552_000);
        assert_eq!((at, opened), (placed(0, 149_500), None));
    }

    #[test]
    fn a_reset_opens_a_new_epoch() {
        let mut clocks = VciClocks::default();
        read(&mut clocks, 3_000_000_000, 10_000);
        // The VCI restarted: a small time a moment later, no wrap could reach it.
        let (at, opened) = read(&mut clocks, 2_000, 20_000);
        assert_eq!(at, placed(1, 2_000));
        let opened = opened.unwrap();
        assert_eq!(
            (opened.epoch, opened.reason),
            (1, EpochReason::BackwardStep)
        );
        assert_eq!(
            opened.previous,
            Some(EndedEpoch {
                epoch: 0,
                us: 3_000_000_000,
                read_us: 10_000
            })
        );
        assert_eq!(
            opened.first,
            EpochTime {
                us: 2_000,
                read_us: 20_000
            }
        );
        // The next time goes on in the new epoch.
        let (at, opened) = read(&mut clocks, 12_000, 30_000);
        assert_eq!((at, opened), (placed(1, 12_000), None));
    }

    /// The times `sim-vci` gave in one job: the transmit was read late, and the pendings and the
    /// response after it in a burst, each ahead of what the first read's delay allowed.
    #[test]
    fn times_read_in_a_burst_after_a_late_read_stay_in_the_epoch() {
        let mut clocks = VciClocks::default();
        read(&mut clocks, 886_982, 151_939);
        for (raw, read_us) in [
            (886_985, 151_962),
            (916_985, 151_983),
            (946_985, 174_555),
            (953_676, 220_310),
            (953_679, 220_310),
        ] {
            let (at, opened) = read(&mut clocks, raw, read_us);
            assert_eq!((at, opened), (placed(0, raw as i64), None), "{raw}");
        }
        // A VCI clock ahead of the agent's: kept too, as a read before it that came late.
        let (at, opened) = read(&mut clocks, 60_000_000, 230_000);
        assert_eq!((at, opened), (placed(0, 60_000_000), None));
        // That moved the alignment: a wrap now needs the agent time to reach it.
        let (_, opened) = read(&mut clocks, 1_000, 240_000);
        assert_eq!(
            opened.map(|opened| opened.reason),
            Some(EpochReason::BackwardStep)
        );
    }

    /// A wrap is unwrapped only when the clock can have run to it since its latest time: by the
    /// agent's time less the least delay, within the drift bound and the clock margin.
    #[test]
    fn a_wrap_needs_the_time_to_reach_it() {
        let wrap_at = |read_us| {
            let mut clocks = VciClocks::default();
            // 10 ms before the wrap, read with no delay.
            read(&mut clocks, SPAN - 10_000, 1_000_000);
            read(&mut clocks, 2_000, read_us)
                .1
                .map(|opened| opened.reason)
        };
        // 12 ms of the VCI's time: the agent must have read it at 1 012 000 or later, give or
        // take the clock margin and 100 ppm of the 11 ms.
        assert_eq!(wrap_at(1_011_000), None);
        assert_eq!(wrap_at(1_010_998), Some(EpochReason::BackwardStep));
    }

    /// Times of one exchange read together across a wrap: the agent's time since the exchange
    /// began bounds how far the clock can have run, so the wrap is unwrapped, while a reset in
    /// the exchange is still a step back.
    #[test]
    fn the_exchange_s_time_bounds_a_wrap_read_together() {
        let in_exchange = Tolerance {
            began_us: Some(40_000),
            ..TOLERANCE
        };
        let together = |raw| {
            let mut clocks = VciClocks::default();
            clocks.read(Clock::Device, SPAN - 5_000, 50_000, 0, in_exchange);
            clocks.read(Clock::Device, raw, 50_000, 0, in_exchange)
        };
        assert_eq!(together(3_000), (placed(0, (SPAN + 3_000) as i64), None));
        // Without the exchange's start the same pair reads as a step back.
        let mut clocks = VciClocks::default();
        read(&mut clocks, SPAN - 5_000, 50_000);
        let (_, opened) = read(&mut clocks, 3_000, 50_000);
        assert_eq!(
            opened.map(|opened| opened.reason),
            Some(EpochReason::BackwardStep)
        );
        // 10 ms into the exchange, the clock cannot have run 20 ms.
        let (_, opened) = together(16_000);
        assert_eq!(
            opened.map(|opened| opened.reason),
            Some(EpochReason::BackwardStep)
        );
    }

    #[test]
    fn a_gap_longer_than_a_wrap_opens_a_new_epoch() {
        let mut clocks = VciClocks::default();
        read(&mut clocks, 1_000, 1_000);
        // Nothing read for more than a wrap: how many passed is unknown.
        let (at, opened) = read(&mut clocks, 2_000, 1_000 + SPAN + 10_000_000);
        assert_eq!(at, placed(1, 2_000));
        assert_eq!(
            opened.map(|opened| opened.reason),
            Some(EpochReason::GapBeyondWrap)
        );
    }

    /// The raw time a clock running `ppm` fast (negative: slow) reads `elapsed_us` of agent
    /// time after `start`.
    fn ran(start: u64, elapsed_us: u64, ppm: i64) -> u64 {
        let run = i128::from(elapsed_us) + i128::from(elapsed_us) * i128::from(ppm) / 1_000_000;
        u64::try_from((i128::from(start) + run).rem_euclid(WRAP)).unwrap()
    }

    /// Half a wrap of the agent's clock with nothing read opens an epoch whatever the value:
    /// one microsecond less is kept (ADR-383).
    #[test]
    fn half_a_wrap_of_silence_opens_an_epoch_whatever_the_value() {
        let half = SPAN / 2;
        let mut clocks = VciClocks::default();
        read(&mut clocks, 1_000, 1_000);
        let (at, opened) = read(&mut clocks, 1_000 + half - 1, 1_000 + half - 1);
        assert_eq!((at, opened), (placed(0, (half - 1 + 1_000) as i64), None));
        let mut clocks = VciClocks::default();
        read(&mut clocks, 1_000, 1_000);
        let (at, opened) = read(&mut clocks, 1_000 + half, 1_000 + half);
        assert_eq!(at, placed(1, (1_000 + half) as i64));
        assert_eq!(
            opened.map(|opened| opened.reason),
            Some(EpochReason::GapBeyondWrap)
        );
    }

    /// A clock faster than the drift bound, read after a gap a little over a wrap or a little
    /// under one, opens an epoch rather than being placed a wrap short; so does a slow one after
    /// more than a wrap (ADR-383).
    #[test]
    fn a_fast_or_slow_clock_after_a_long_gap_opens_an_epoch() {
        for (elapsed_us, ppm) in [
            (SPAN + 10_000_000, 200),
            (SPAN - 1_000_000, 300),
            (SPAN + 10_000_000, -200),
        ] {
            let mut clocks = VciClocks::default();
            read(&mut clocks, 1_000, 1_000);
            let raw = ran(1_000, elapsed_us, ppm);
            let (at, opened) = read(&mut clocks, raw, 1_000 + elapsed_us);
            assert_eq!(at, placed(1, raw as i64), "{elapsed_us} {ppm}");
            assert_eq!(
                opened.map(|opened| opened.reason),
                Some(EpochReason::GapBeyondWrap),
                "{elapsed_us} {ppm}"
            );
        }
    }

    /// The silence runs from the latest read of a time kept in the epoch, a time just behind
    /// the latest one included.
    #[test]
    fn a_silence_runs_from_the_latest_read_kept_in_the_epoch() {
        let half = SPAN / 2;
        let mut clocks = VciClocks::default();
        read(&mut clocks, 10_000, 10_000);
        // Behind the latest within the margin, read a second later: kept, and read since.
        let (_, opened) = read(&mut clocks, 9_500, 1_010_000);
        assert_eq!(opened, None);
        let (_, opened) = read(&mut clocks, 10_000 + half, 1_010_000 + half - 1);
        assert_eq!(opened, None);
    }

    /// A time with no read time of its own, and the latest time that orders times read
    /// together, decide nothing once the agent has read nothing on the clock for half a wrap.
    #[test]
    fn nothing_is_placed_nearest_a_latest_time_half_a_wrap_old() {
        let half = SPAN / 2;
        let mut clocks = VciClocks::default();
        read(&mut clocks, 1_000, 1_000);
        assert_eq!(
            clocks.near_latest(Clock::Device, 2_000, 1_000 + half - 1),
            Some(placed(0, 2_000))
        );
        assert_eq!(clocks.near_latest(Clock::Device, 2_000, 1_000 + half), None);
        assert_eq!(clocks.latest(Clock::Device, 1_000 + half - 1), Some(1_000));
        assert_eq!(clocks.latest(Clock::Device, 1_000 + half), None);
    }

    #[test]
    fn a_reconnect_opens_a_new_epoch_on_each_clock_that_had_one() {
        let mut clocks = VciClocks::default();
        read(&mut clocks, 1_000, 1_000);
        clocks.reconnect();
        // A clock with no epoch yet has nothing to carry on from.
        let (_, worker) = clocks.read(Clock::Worker, 5, 2_000, 0, TOLERANCE);
        assert_eq!(worker.map(|opened| opened.reason), Some(EpochReason::First));
        // The device clock carried on, but nothing shows it yet.
        assert_eq!(clocks.near_latest(Clock::Device, 1_500, 2_000), None);
        let (at, opened) = read(&mut clocks, 1_500, 2_000);
        assert_eq!(at, placed(1, 1_500));
        assert_eq!(
            opened.map(|opened| opened.reason),
            Some(EpochReason::Reconnect)
        );
        let (_, opened) = read(&mut clocks, 2_500, 3_000);
        assert_eq!(opened, None);
    }

    #[test]
    fn epochs_go_on_from_those_the_log_holds() {
        let mut clocks = VciClocks::starting_at([4, 2]);
        let (at, _) = read(&mut clocks, 5_000, 1_000);
        assert_eq!(at.epoch, 4);
        let (at, _) = clocks.read(Clock::Worker, 1_000, 1_000, 0, TOLERANCE);
        assert_eq!(at.epoch, 2);
        let (at, _) = read(&mut clocks, 10, 2_000);
        assert_eq!(at.epoch, 5);
    }

    #[test]
    fn a_time_with_no_read_time_is_placed_nearest_the_latest() {
        let mut clocks = VciClocks::default();
        assert_eq!(clocks.near_latest(Clock::Device, 5, 0), None);
        read(&mut clocks, SPAN - 1_000, 1_000);
        // Just after the wrap, and just before the latest time.
        assert_eq!(
            clocks.near_latest(Clock::Device, 500, 1_500),
            Some(placed(0, (SPAN + 500) as i64))
        );
        assert_eq!(
            clocks.near_latest(Clock::Device, SPAN - 3_000, 1_500),
            Some(placed(0, (SPAN - 3_000) as i64))
        );
        // It does not move the clock's state.
        let (at, _) = read(&mut clocks, SPAN - 500, 1_600);
        assert_eq!(at, placed(0, (SPAN - 500) as i64));
    }

    #[test]
    fn a_time_an_item_carries_comes_at_or_before_its_end() {
        let end = placed(0, (SPAN + 2_000) as i64);
        // The transmit done before the wrap, the start after it.
        assert_eq!(before(end, SPAN - 4_000), placed(0, (SPAN - 4_000) as i64));
        assert_eq!(before(end, 1_000), placed(0, (SPAN + 1_000) as i64));
        assert_eq!(before(end, 2_000), end);
        // Before the epoch's first time, below it.
        assert_eq!(before(placed(2, 1_000), SPAN - 500), placed(2, -500));
    }

    /// A gap of 31.2 s at 100 ppm: a tolerance of 2 ms for the two readings plus 3.12 ms.
    const GAP: u64 = 31_200_000;

    #[test]
    fn a_clock_that_carried_on_reads_its_last_time_plus_the_gap() {
        let fit = loss_fit(1_835_511_040, 1_866_711_040 + 4_000, GAP, 0, 100, 1_000);
        assert_eq!(fit.expected_us, 1_866_711_040);
        assert_eq!(fit.tolerance_us, 5_120);
        assert!(fit.carried_on && !fit.restarted, "{fit:?}");
        // Just past the tolerance on either side, it carried on no longer.
        for reading in [1_866_711_040 + 5_121, 1_866_711_040 - 5_121] {
            assert!(!loss_fit(1_835_511_040, reading, GAP, 0, 100, 1_000).carried_on);
        }
    }

    #[test]
    fn the_reading_s_round_trip_widens_the_tolerance() {
        // The clock was read up to 3 ms before its answer came.
        let fit = loss_fit(1_835_511_040, 1_866_711_040 - 8_000, GAP, 3_000, 100, 1_000);
        assert_eq!(fit.tolerance_us, 8_120);
        assert!(fit.carried_on && !fit.restarted, "{fit:?}");
        assert!(!loss_fit(1_835_511_040, 1_866_711_040 - 8_121, GAP, 3_000, 100, 1_000).carried_on);
    }

    #[test]
    fn a_clock_that_restarted_reads_no_more_than_the_gap() {
        let fit = loss_fit(1_835_511_040, 2_000_000, GAP, 0, 100, 1_000);
        assert!(!fit.carried_on && fit.restarted, "{fit:?}");
        assert!(loss_fit(1_835_511_040, GAP + 5_120, GAP, 0, 100, 1_000).restarted);
        assert!(!loss_fit(1_835_511_040, GAP + 5_121, GAP, 0, 100, 1_000).restarted);
    }

    #[test]
    fn a_clock_whose_last_time_plus_the_gap_reaches_the_wrap_fits_both() {
        // The expected value wraps to 10 s, within the gap a restarted clock may have run.
        let last = SPAN - GAP + 10_000_000;
        let fit = loss_fit(last, 10_000_000, GAP, 0, 100, 1_000);
        assert_eq!(fit.expected_us, 10_000_000);
        assert!(fit.carried_on && fit.restarted, "{fit:?}");
        // And one a few microseconds before the wrap is still near the expected value.
        let fit = loss_fit(SPAN - 1_000, 500, 0, 0, 100, 1_000);
        assert!(fit.carried_on, "{fit:?}");
    }

    #[test]
    fn a_reading_far_from_both_fits_neither() {
        let fit = loss_fit(1_835_511_040, 900_000_000, GAP, 0, 100, 1_000);
        assert!(!fit.carried_on && !fit.restarted, "{fit:?}");
    }

    #[test]
    fn the_last_time_is_the_latest_raw_time_and_when_it_was_read() {
        let mut clocks = VciClocks::default();
        assert_eq!(clocks.last(Clock::Device), None);
        read(&mut clocks, 5_000, 7_000);
        read(&mut clocks, 6_000, 8_100);
        assert_eq!(
            clocks.last(Clock::Device),
            Some(EndedEpoch {
                epoch: 0,
                us: 6_000,
                read_us: 8_100
            })
        );
        assert_eq!(clocks.last(Clock::Worker), None);
    }
}
