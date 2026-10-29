# ADR-383: Half a Wrap of Silence Opens a Clock Epoch

**Date:** 2026-10-29
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log/clocks.rs` `VciClocks::read`,
`VciClocks::near_latest`, `VciClocks::latest`, `EpochReason`; `comm_log.rs`
`CommLog::exchange_with`'s order anchor, `Inner::place_read`, `Inner::place_unread`); design
8.11 ("Drift bound", "Window", "Order", `clock_epoch`); `crates/agent/docs/ngr-agent.md`;
ADR-361 items 2 and 3; ADR-375; ADR-378

## Context

ADR-361 unwraps each VCI clock's 32-bit microsecond times within a window of the agent's clock.
A time takes the first value at or above the clock's latest time less the clock margin, and is
checked against the clock's reach: the agent's read time less the least delay seen, plus the
drift bound over the agent time since then, plus the clock margin. A value a wrap later still
within the reach opened a new epoch as a gap beyond the wrap (item 2). Otherwise the value was
kept with the wrap count of the first candidate.

So after a long silence the reach decided how many wraps had passed, and the reach can fall
short in realistic cases:

- With no profile, the drift bound and clock margin are the agent's defaults (100 ppm, 1 ms,
  ADR-357), which stand in for an error no API reports (ADR-375 item 2). A clock 200 ppm fast,
  read 10 s more than a wrap (about 71.6 minutes) after its latest time, lies about 0.4 s past
  the reach and is kept one wrap short in the same epoch. Its times are then compared falsely.
- The same happens with a profile's values. The least delay can be too high: ADR-361's
  Consequences note a transmit indication read 500 ms late on the first job against `sim-vci`.
  The reach then falls short by that much, and a clock at its stated rate can lie past it.

ADR-378 judged a loss only on tolerances a profile gives and left the placement to a change of
its own. Using only a profile's tolerances would not close the second case. A threshold of one
wrap of agent time would leave the window just under a wrap for any fast clock. ADR-375 rules
out a fix that closes only the case found (item 4).

Any threshold on the agent's time is a bound on the clock's rate: a clock at rate 1 + r runs a
wrap in W / (1 + r) of agent time. The maintainer chose (2026-10-29) half a wrap, the line
ADR-361 item 3 already draws by placing a time nearest the latest within half a wrap.

## Decision

1. **Half a wrap of silence opens an epoch.** When the agent has read nothing on a clock for
   half a wrap of its own time (2^31 µs, about 35.8 minutes), the next time read on it opens a
   new epoch with `gap_beyond_wrap`, whatever its value. The silence runs from the agent's
   latest read of a time kept in the epoch, so a time just behind the latest one that is read
   later counts too. The old check, a value a wrap later still within the reach, is removed.

2. **The model behind it.** The log follows a clock that is a microsecond counter running at
   less than twice the agent's rate, whose times are read far less than half a wrap late. This
   is stated in design 8.11 as an assumption of the model, not as a tolerance chosen so that a
   comparison decides. ISO 22900-2:2022 8.1.6.1 describes the time base as a microsecond
   counter, and no oscillator is off by a factor near two. Within half a wrap such a clock
   cannot have run a full wrap unseen. No judgement of the log compares two times that far
   apart: timing is judged within one exchange, and settlements use raw times.

3. **The tolerances tell a wrap from a step back.** The drift bound and the clock margin, a
   profile's or the defaults, still set the reach and the margin behind the latest time. A value
   past a wrap beyond the reach opens an epoch with `backward_step`: a step back, or a run
   faster than the tolerances cover. They never decide how many wraps a silence holds.

4. **Times placed without a read of their own follow the same line.**
   - A time read at no known agent time is placed nearest the clock's latest time only when the
     agent read something on the clock within half a wrap before the latest time the time can
     have been read: the exchange's end, or the agent's time now. Otherwise it is read as at
     that time, and opens the epoch.
   - Times read together are ordered nearest the clock's latest time only when it lies within
     half a wrap before the first of them. Otherwise the first of them anchors the order. Two
     items read together on each side of the old latest time's opposite point then open one
     epoch, not a step back.

## Consequences

- No time is placed with a false wrap count for a clock at less than twice the agent's rate
  and reads less than half a wrap late, whatever the drift bound, the clock margin or the
  least delay. The defaults decide only whether an epoch goes on at a wrap, never a
  comparison's value (ADR-378).
- A link idle for 36 minutes opens an epoch at its next time. No judgement spans such an idle,
  so nothing is lost. An exchange whose primitive waited that long has its times in two epochs,
  and its timing is not judged.
- ADR-361's accepted limitation stands: a reset that lands within the reach reads as a wrap.
- A clock the model does not describe is outside this rule: one at twice the agent's rate or
  more, or a profile whose drift bound or clock margin approaches half a wrap.
