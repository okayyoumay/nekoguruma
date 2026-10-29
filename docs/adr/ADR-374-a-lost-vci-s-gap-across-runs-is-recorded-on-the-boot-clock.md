# ADR-374: A Lost VCI's Gap Across Runs Is Recorded on the Boot Clock

**Date:** 2026-10-28
**Status:** Accepted (item 2's missing samples dropped where the offset shows no suspend, item 4's round trip unbounded with no boot time at either end, item 5's gap not judged, now also because no suspend count decides it, the Consequences' later judgement and kept epoch waiting on evidence that no system gives, and their growing list of offsets bounded, by ADR-379)
**Affects:** `agent` communication log (`comm_log/boot.rs`; `comm_log.rs` `VciLoss`, `LossBoot`,
`LossTime`, `LossClock`, `ConnectReading`, `CommLog::now`, `CommLog::round_trip_us`,
`Inner::note_loss`, `Inner::settle_loss`, `Inner::sample_boot_at`, `Inner::boot_at`,
`Inner::slept_at`; `comm_log/clocks.rs` `loss_fit`), `agent` runner (`run_job`'s clock read at
connect); design 8.11 (`vci_loss`, `vci_loss_settled`); ADR-373 (its first Consequence)

## Context

ADR-373 settles a lost VCI in the next run whose VCI connects. Its comparison needs the time
that passed between the clock's last time before the loss and the reading after the reconnect.
It measured that time on the agent's monotonic clock, which starts over with every run, so a
gap was measured only within one agent epoch. Every reopen of the log starts a new epoch, and
the run that reconnects is always a later run. So every loss settled as `undetermined` with
`power_kept` `unknown`, and nothing in the log said how long the loss had lasted.

A clock that both runs share would measure the gap. It must run through a suspend of the PC,
since the VCI's own clock goes on while the PC sleeps, and the log must know that both readings
come from the same boot of the machine, since such a clock starts over at a reboot.

Measuring the gap so turned out not to be enough to judge a loss from it. Review of the work
found, one after another, cases where a judgement on such a gap would be false either way: a
suspend's sleep, which the kernel measures only to about a second; a VCI clock past its 32-bit
wrap, or just after its start, reading as the other outcome within that error; the device
clock's last time read only after a resume; a short suspend the kernel records as no sleep at
all. Each bound or check that closed one case left another. The maintainer chose (2026-10-29)
to record what a later judgement needs and to judge nothing from it yet.

## Decision

1. **The boot clock.** The log takes the machine's boot clock and boot identifier from the
   system. On Linux these are `CLOCK_BOOTTIME`, which counts from the boot and through
   suspend, and the kernel's boot id (`/proc/sys/kernel/random/boot_id`). On other systems the
   log has neither, and a gap across runs stays unmeasured: the maintainer chose Linux only
   (2026-10-28). The agent's own clock stays the monotonic one, and exchange entries carry no
   boot time. The log reads the boot clock beside each agent time it takes (item 2), at the
   cost of a few more clock reads per time, so that a loss's times can be set on it.

2. **The boot times.** The agent's monotonic clock stops during a suspend and the boot clock
   does not, so the offset between them grows with each suspend. Whenever the log reads the
   agent's clock it also reads the boot clock, between two further reads of the agent's clock,
   and it keeps each growth of the offset with the agent time it was seen at. A read whose two
   agent readings lie further apart than the reads' jitter (50 µs: the thread was preempted
   between them) is tried at most three times in all, so that a scheduling delay is not taken
   for a suspend. When every try is delayed, the log marks the sample as missing: a time read
   from then to the next sample has no boot time, since a suspend may have come since the last
   one. A time's boot time is its agent time plus the offset in force when it was read, so a
   suspend between the read and the time's placement, or before the loss is noted, does not
   skew it.

3. **The record.** The first `vci_loss` record of a loss keeps `boot`: the boot id, the time on
   the boot clock at which the device clock's last time was read, and how long the machine had
   slept since its boot then (`slept_us`: the boot time less the system's monotonic time, which
   stops during a suspend). The system clock's offset from the agent's is constant, so it is
   read once, between two reads of the agent's clock as a boot sample is. A record without a
   device time, written on a system without a boot clock, or whose device time was read where
   a sample was missing, has no `boot`.

4. **The settlement's record.** When the record's boot id is the reconnecting run's, the
   settlement measures the gap on the boot clock, whatever the agent epochs, and records it in
   `vci_loss_settled`. The record holds:
   - which clock measured the gap (`gap_from`, `boot` or `agent`) and the gap (`gap_us`,
     which replaces `agent_gap_us`);
   - whether the boots match (`boot_match`, `null` when the record has no `boot` or the
     reconnecting run has no boot id);
   - each read's boot time and sleep figure (`boot_us`, `slept_us`), and how much longer the
     machine had slept by the reconnect (`slept_in_gap_us`);
   - the reading's round trip (`round_trip_us`), and the comparison of design 8.11 that the
     gap gives.

   The runner times its `GetTimestamp` at the reconnect, on the boot clock when there is one,
   so that a suspend during the read counts. The reading's agent time is taken after the
   answer, so the VCI's clock was read at most that round trip earlier, and the comparison's
   tolerance adds the round trip to twice the clock margin and the drift bound over the gap.
   When only one end of the read has a boot time (a sample there was missing), nothing bounds
   the round trip. The reading is then compared with nothing: `round_trip_us` is `null` and
   `power_kept` `unknown`.

5. **No judgement from it.** A gap on the boot clock settles a loss as `power_kept` `unknown`,
   whatever its comparison gives, and the reason says that the gap is recorded but not judged.
   A gap on the agent's clock, within one agent epoch, is judged as ADR-373 says, with the
   round trip of item 4 in its tolerance and no comparison when the round trip is unbounded;
   no settlement reaches it today, since every reopen starts a new agent epoch.

## Consequences

- Nothing settles differently. Every loss a later run settles stays `undetermined` with
  `power_kept` `unknown`, as under ADR-373, but its `vci_loss_settled` now carries the gap
  since the loss and the sleep in it, when one boot of a Linux machine holds both runs. A
  later change can judge from those figures. It needs a bound on the sleep's error (the
  kernel's resume path, per platform), the read delay of the device clock's last time,
  and a rule for a gap with a suspend in it, which is follow-up work.
- A clock that carried on still opens a new epoch at the reconnect (design 8.11 "Reconnect").
  Keeping it waits for that judgement.
- The log keeps one entry per growth of the offset, one per run of missing samples, and the
  first sample after such a run. On a machine whose clock reads are slow enough to miss the
  jitter bound often (some virtual machines), the entries grow with the job's clock reads;
  nothing prunes the ones no time needs any more.
- The figures assume both runs see one boot clock. On Linux a time namespace (a container
  restored onto another host's boot, for example) can offset `CLOCK_BOOTTIME` while the boot
  id stays the host's, and the figures do not hold there.
- The boot id is a random identifier of one boot of the machine. It names no person and no
  vehicle, and it lives in the job's own log beside the times it relates.
