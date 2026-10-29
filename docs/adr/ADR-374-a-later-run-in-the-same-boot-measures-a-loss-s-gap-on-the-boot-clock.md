# ADR-374: A Later Run in the Same Boot Measures a Loss's Gap on the Boot Clock

**Date:** 2026-10-28
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log/boot.rs`; `comm_log.rs` `VciLoss`, `LossBoot`,
`LossClock`, `ConnectReading`, `Inner::note_loss`, `Inner::settle_loss`; `comm_log/clocks.rs`
`loss_fit`), `agent` runner (`run_job`'s clock read at connect); design 8.11 (`vci_loss`,
`vci_loss_settled`); ADR-373 (Decision item 4's gap within one agent epoch, and its first
Consequence)

## Context

ADR-373 settles a lost VCI in the next run whose VCI connects. Its comparison needs the time
that passed between the clock's last time before the loss and the reading after the reconnect.
It measured that time on the agent's monotonic clock, which starts over with every run, so a
gap was measured only within one agent epoch. Every reopen of the log starts a new epoch, and
the run that reconnects is always a later run. So every loss settled as `undetermined` with
`power_kept` `unknown`, even when the VCI kept its power.

A clock that both runs share would measure the gap. It must run through a suspend of the PC,
since the VCI's own clock goes on while the PC sleeps, and the log must know that both readings
come from the same boot of the machine, since such a clock starts over at a reboot.

## Decision

1. **The boot clock.** The log takes the machine's boot clock and boot identifier from the
   system. On Linux these are `CLOCK_BOOTTIME`, which counts from the boot and through
   suspend, and the kernel's boot id (`/proc/sys/kernel/random/boot_id`). On other systems the
   log has neither, and a gap across runs stays unmeasured: the maintainer chose Linux only
   (2026-10-28). The agent's own clock stays the monotonic one, and exchange entries carry no
   boot time. The log reads the boot clock beside each agent time it takes (item 2), at the
   cost of a few more clock reads per time, so that a loss's times can be set on it.

2. **The record.** The first `vci_loss` record of a loss keeps `boot`: the boot id and the time
   on the boot clock at which the device clock's last time was read. The agent's monotonic clock
   stops during a suspend and the boot clock does not, so the offset between them grows with
   each suspend. Whenever the log reads the agent's clock it also reads the boot clock, between
   two further reads of the agent's clock, and it keeps each growth of the offset with the agent
   time it was seen at. A read whose two agent readings lie further apart than the reads'
   jitter (50 µs: the thread was preempted between them) is tried at most three times in all,
   so that a scheduling delay is not taken for a suspend. When every try is delayed, the log
   marks the sample as missing: a time read from then to the next sample has no boot time, since
   a suspend may have come since the last one. A time's boot time is its
   agent time plus the offset in force when it was read, so a suspend between the read and the
   time's placement, or before the loss is noted, does not skew it. A record without a device
   time, or written on a system without a boot clock, has no `boot`.

3. **The gap.** The settlement measures the gap on the boot clock when the record's boot id is
   the reconnecting run's, whatever the agent epochs; otherwise, as before, on the agent's clock
   when both reads lie in one agent epoch; otherwise not at all. The reading's boot time is
   derived the same way as the record's. `vci_loss_settled` records which clock measured the
   gap (`gap_from`, `boot` or `agent`), the gap (`gap_us`, which replaces `agent_gap_us`),
   whether the boots match (`boot_match`, `null` when the record has no `boot` or the
   reconnecting run has no boot id), and each
   read's boot time (`boot_us`), so a reader can redo it. A different boot leaves the loss
   `undetermined`, as a gap across agent epochs did.

4. **The reading's round trip.** The runner times its `GetTimestamp` at the reconnect, on the
   boot clock when there is one so that a suspend during the read counts, and the reading's
   agent time is taken after the answer, so the VCI's clock was read at most that round trip
   earlier. The tolerance of the comparison adds the round trip
   (`round_trip_us` in `vci_loss_settled`) to twice the clock margin and the drift bound over
   the gap. When only one end of the read has a boot time (a sample there was missing),
   the agent's clock could leave out a suspend in between, so nothing bounds the round trip:
   the reading is then compared with nothing, `round_trip_us` is `null` and `power_kept` is
   `unknown`.

5. **A gap with a suspend in it is compared with nothing.** A gap that spans a suspend of the
   PC is only as exact as the kernel's measure of the sleep (Consequences), and a clock that
   carried on past its wrap, or one that started again just after the clock's start, can read
   within that error of the other outcome, so neither a kept power nor a power loss can be
   shown from such a gap. The log therefore records, beside each boot time, how long the
   machine had slept since its boot: the boot clock less the system's monotonic clock, which
   stops during a suspend (`slept_us` in the record and in each read of `vci_loss_settled`).
   The system clock's offset from the agent's is read once, between two reads of the agent's
   clock as a boot clock sample is (item 2), so that a delay does not pass for a suspend.
   When the reconnect's figure exceeds the loss's by more than the reads' jitter (1 ms), or a
   boot clock gap has no such figure, `vci_loss_settled` records the difference
   (`slept_in_gap_us`) and compares nothing: `power_kept` is `unknown`. A gap without a suspend
   is exact, on the boot clock as on the agent's, and settles as ADR-373 says, `false`
   included. The maintainer chose this (2026-10-29) over bounding the sleep's error. ADR-373
   measured no gap across runs, so the case did not arise before.

## Consequences

- On Linux a loss that a later run of the same boot settles, on a link whose worker reads the
  VCI's own clock and gives a serial number (a D-PDU link), can now be shown to have kept its
  power, and its exchanges become `system`. A reboot in between still leaves it `unknown`.
- A clock shown to have carried on still opens a new epoch at the reconnect (design 8.11
  "Reconnect"): keeping it needs the clock's state from the loss record, which is follow-up
  work. So is allowing for the read delay of the clock's last time before the loss (its read
  time is when the host read its largest raw time, which a batch read can delay), which the
  tolerance does not yet take in.
- A gap that spans a suspend of the PC is only as exact as the kernel's measure of the sleep.
  A machine whose clock source stops in suspend (most x86 machines) adds the sleep time from
  the real-time clock, which counts whole seconds, and the kernel's adjustment between that
  clock and the system time can add more. So a VCI lost while the PC slept, or before a
  suspend that came before the reconnect, settles as `unknown` (item 5), however it fared:
  the common case of a laptop that suspends while the VCI keeps its power is not shown kept.
  Settling such a loss needs a bound on the sleep's error, which is follow-up work.
- The log keeps one entry per growth of the offset, one per run of missing samples, and the
  first sample after such a run. On a machine whose
  clock reads are slow enough to miss the jitter bound often (some virtual machines), the
  entries grow with the job's clock reads; nothing prunes the ones no time needs any more.
- The comparison assumes both runs see one boot clock. On Linux a time namespace (a container
  restored onto another host's boot, for example) can offset `CLOCK_BOOTTIME` while the boot id
  stays the host's, and the comparison does not hold there.
- The boot id is a random identifier of one boot of the machine. It names no person and no
  vehicle, and it lives in the job's own log beside the times it relates.
