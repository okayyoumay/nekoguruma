//! The VCI's clocks as the communication log follows them (design 8.11 "Timing judged",
//! `clock_epoch`, ADR-361): each clock's 32-bit microsecond times aligned with the agent's
//! monotonic clock and unwrapped, and a new epoch opened where the time base did not carry on.
//!
//! A clock's alignment is the least delay seen in its epoch between a VCI time and the agent
//! reading the item that carried it. A time can have been taken no later than the agent read it,
//! less that least delay, give or take the drift bound over the agent time since that least
//! delay was seen plus the clock margin (design 8.11 "Drift bound"), and no earlier than the
//! clock's latest time, less the clock margin. A raw time is unwrapped to the one value in that
//! window; none (the time stepped back without wrapping, or ahead of the agent's clock) or more
//! than one (the agent read nothing on the clock for longer than a wrap) opens a new epoch, and
//! so does the first time read after the VCI reconnected.

use serde::Serialize;

use super::exchange::Clock;

/// The span of the VCI's 32-bit microsecond times (ISO 22900-2:2022 8.1.6.1).
const WRAP: i128 = 1 << 32;

/// Why a clock's epoch began (`clock_epoch`'s `reason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EpochReason {
    /// The clock's first time in this run of the log.
    First,
    /// The time lies behind the clock's latest one, by more than a wrap could explain.
    BackwardStep,
    /// The time lies ahead of when the agent read it, beyond the tolerance.
    ForwardStep,
    /// The agent read nothing on the clock for longer than a wrap, so how many wraps passed is
    /// unknown.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
            (Some(mut epoch), false) => {
                let raw = i128::from(raw);
                let upper = i128::from(read_us) - epoch.least_delay
                    + tolerance.over(read_us.saturating_sub(epoch.least_at_us));
                let lower = epoch.latest.unwrapped - i128::from(tolerance.clock_margin_us);
                // The wraps that put the time in the window: from the first at or above
                // `lower` to the last at or below `upper`.
                let first = -(raw - lower).div_euclid(WRAP);
                let last = (upper - raw).div_euclid(WRAP);
                match last - first {
                    0 => {
                        let unwrapped = raw + first * WRAP;
                        if unwrapped > epoch.latest.unwrapped {
                            epoch.latest = reading(unwrapped);
                        }
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
                    gap if gap > 0 => EpochReason::GapBeyondWrap,
                    // No value fits: a raw time below the latest one stepped back without
                    // wrapping, one above it stepped ahead of the agent's clock.
                    _ if raw < i128::from(epoch.latest.raw) => EpochReason::BackwardStep,
                    _ => EpochReason::ForwardStep,
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

    /// Places `raw`, a time on `clock` with no read time of its own, nearest the clock's latest
    /// time, without taking it into the clock's state. `None` when the clock has no epoch, or
    /// the VCI connected again since its latest time.
    pub(crate) fn near_latest(&self, clock: Clock, raw: u64) -> Option<Placed> {
        let state = match clock {
            Clock::Device => &self.device,
            Clock::Worker => &self.worker,
        };
        let epoch = state.current.filter(|_| !state.reconnected)?;
        let raw = i128::from(raw);
        let k = (epoch.latest.unwrapped - raw + WRAP / 2).div_euclid(WRAP);
        Some(Placed {
            epoch: epoch.number,
            unwrapped: i64::try_from(raw + k * WRAP).ok()?,
        })
    }
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

    #[test]
    fn a_time_ahead_of_the_agent_opens_a_new_epoch() {
        let mut clocks = VciClocks::default();
        read(&mut clocks, 1_000, 1_000);
        // 10 ms of agent time, but the VCI's time moved 5 s.
        let (at, opened) = read(&mut clocks, 5_001_000, 11_000);
        assert_eq!(at, placed(1, 5_001_000));
        assert_eq!(
            opened.map(|opened| opened.reason),
            Some(EpochReason::ForwardStep)
        );
        // Ahead by no more than the tolerance stays: 1 ms margin plus 100 ppm of 10 s.
        let mut clocks = VciClocks::default();
        read(&mut clocks, 1_000, 1_000);
        let (at, opened) = read(&mut clocks, 10_003_000, 10_001_000);
        assert_eq!((at, opened), (placed(0, 10_003_000), None));
        let mut clocks = VciClocks::default();
        read(&mut clocks, 1_000, 1_000);
        let (_, opened) = read(&mut clocks, 10_003_001, 10_001_000);
        assert_eq!(
            opened.map(|opened| opened.reason),
            Some(EpochReason::ForwardStep)
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

    #[test]
    fn a_reconnect_opens_a_new_epoch_on_each_clock_that_had_one() {
        let mut clocks = VciClocks::default();
        read(&mut clocks, 1_000, 1_000);
        clocks.reconnect();
        // A clock with no epoch yet has nothing to carry on from.
        let (_, worker) = clocks.read(Clock::Worker, 5, 2_000, 0, TOLERANCE);
        assert_eq!(worker.map(|opened| opened.reason), Some(EpochReason::First));
        // The device clock carried on, but nothing shows it yet.
        assert_eq!(clocks.near_latest(Clock::Device, 1_500), None);
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
        assert_eq!(clocks.near_latest(Clock::Device, 5), None);
        read(&mut clocks, SPAN - 1_000, 1_000);
        // Just after the wrap, and just before the latest time.
        assert_eq!(
            clocks.near_latest(Clock::Device, 500),
            Some(placed(0, (SPAN + 500) as i64))
        );
        assert_eq!(
            clocks.near_latest(Clock::Device, SPAN - 3_000),
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
}
