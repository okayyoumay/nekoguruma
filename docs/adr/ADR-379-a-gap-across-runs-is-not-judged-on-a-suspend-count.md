# ADR-379: A Gap Across Runs Is Not Judged on a Suspend Count

**Date:** 2026-10-29
**Status:** Accepted
**Affects:** design 8.11 (`vci_loss_settled`); `agent` communication log (`comm_log.rs`
`CommLog::round_trip_us`, `Inner::note_loss`, `Inner::settle_loss`, `Inner::sample_boot_at`,
`Inner::boot_at`); `crates/agent/docs/ngr-agent.md`; ADR-374 items 2, 4 and 5 and its
Consequences

## Context

ADR-374 measures a lost VCI's gap across two runs of one boot on the boot clock and records it,
but judges nothing from it (item 5): across a suspend the kernel measures the sleep only to
about a second, and a VCI clock past its wrap, or just after its start, can then read as the
other outcome. Its Consequences left the judgement to later work. The maintainer chose
(2026-10-29) to try one piece of evidence for that work. The log would read the kernel's count
of completed suspends at the loss and at the reconnect, and judge the gap only when both counts
were read and are equal, so that no suspend can have come in it.

On Linux that count is `success` in `/sys/power/suspend_stats` (`Documentation/ABI/testing/
sysfs-power`). Checked against the kernel's code (`kernel/power/suspend.c` `pm_suspend`,
`kernel/power/main.c` `dpm_save_errno`), it counts every suspend whose entry succeeded, a
suspend under a second included. It does not decide the gap, for two reasons:

- **Hibernation is left out.** Only the suspend path adds to the count; the hibernation path
  (`kernel/power/hibernate.c`) does not. A machine that hibernates and resumes keeps its boot
  id and its boot clock runs on across the sleep, so the gap is on the boot clock, its sleep is
  measured no better than a suspend's, and the count is unchanged.
- **The count grows only after processes run again.** `pm_suspend` adds to the count after
  `enter_state` returns, and `enter_state` thaws the machine's processes before it returns. A
  run that reads the count just after a resume can read it from before the suspend. Nothing a
  process can read bounds that window: the kernel takes no lock it shares with the count's
  read, and a later read only narrows the window.

Narrowing to machines where hibernation cannot happen for the whole boot would close the first
reason, but not the second. ADR-375 rules out an outcome that a realistic case consistent with
the evidence would make false, however rare (item 1), and a fix that closes only the case found
(item 4). The maintainer chose (2026-10-29) not to judge the gap, and not to record the counts.

ADR-374's Consequences also noted that the log's list of boot clock offsets grows with every
missed sample on a machine whose clock reads often miss the jitter bound.

## Decision

1. **No judgement of a gap across runs.** A gap on the boot clock between two runs of one boot
   stays as ADR-374 item 5 says: recorded with the comparison it gives, `power_kept`
   `unknown`. The reason is now twofold: the sleep's measurement, and that nothing the system
   gives shows that no suspend came in the gap. The suspend count is not read and not recorded,
   since it decides nothing and a reader can do nothing with it the log cannot.

2. **A missing sample that the offsets on both sides show had no suspend in it is dropped.**
   The offset between the boot clock and the agent's clock grows only across a suspend. When a
   sample follows a run of missing ones (ADR-374 item 2) and its offset exceeds the one before
   the run by no more than the jitter, no suspend came in the run. The run's mark is then
   removed and no new entry is added, so a time read during the run takes the earlier offset,
   as one read between two samples does. A run whose next sample shows the offset grown keeps
   its mark: its times have no boot time, as before. A figure that looks up several boot times
   (a round trip, a loss's `boot`, a settlement's reads) takes one sample first and looks every
   time up in the same list, so that a sample taken between two lookups cannot give them
   different offsets.

3. **A round trip with no boot time at either end is unbounded.** On a machine with a boot
   clock, a round trip whose two ends both lie in a run of missing samples has no boot time at
   either end, and the agent's clock would leave out a suspend between them. It is `null`, as
   one with a boot time at one end only is (ADR-374 item 4). Only a system without a boot clock
   measures a round trip on the agent's clock alone.

## Consequences

- Nothing settles differently. Every loss a later run settles stays `undetermined` with
  `power_kept` `unknown`. A clock that carried on still opens a new epoch at the reconnect
  (design 8.11 "Reconnect"). Keeping it waits for evidence that decides whether the machine
  slept in the gap, which no system the agent runs on gives today.
- The list of offsets now holds one entry for each growth of the offset, and for each run of
  missing samples in which it grew. Its length follows the machine's suspends, not the job's
  clock reads.
- A time read during a dropped run gets a boot time it did not have when it was read, if it is
  placed after the next sample. A figure the log wrote before that sample (a round trip, a
  loss's `boot`) keeps the open value it was written with. Both are true of the evidence at
  the time each was written.
