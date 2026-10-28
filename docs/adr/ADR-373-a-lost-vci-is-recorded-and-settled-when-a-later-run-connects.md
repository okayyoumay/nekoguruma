# ADR-373: A Lost VCI Is Recorded and Settled When a Later Run Connects

**Date:** 2026-10-28
**Status:** Accepted
**Affects:** `agent` communication log (`comm_log.rs` `VciLoss`, `Inner::note_loss`,
`Inner::settle_loss`, `CommLog::vci_connected`, `CommLog::loss_to_settle`, `scan`;
`comm_log/clocks.rs` `VciClocks::last`, `loss_fit`), `agent` runner (`run_job`) and host
(`read_vci_clock`); design 8.11 ("Settlement", `vci_loss_settled`, "Reconnect"); ADR-294,
ADR-346, ADR-357, ADR-361

## Context

Design 8.11 settles a lost VCI in `vci_loss_settled`. The first reading of the VCI's own clock
after it reconnects is set against the gap the agent's clock measured since the clock's last time
before the loss. A clock that carried on shows that the VCI kept its power, so the loss was the
PC's connection to the VCI (`system`). A clock that restarted, or a reading that fits both or
neither, leaves the loss `undetermined`.

The agent does not reconnect within a run. A lost VCI ends the run's work on the link, and a
later run of the job reconnects. The log is not read back into a later run (ADR-346): each run
starts its link state and its clocks afresh. So the later run needs the earlier run's facts in a
form it can find. The maintainer decided (2026-10-27) that the run that loses the VCI writes a
compact record of only what the settlement needs, and that the reconnecting run reads that
record alone.

## Decision

1. **The record.** The run that loses the VCI writes a `vci_loss` event. It holds:
   - `loss`, the `seq` of the loss's first record;
   - the agent epoch of the run;
   - `ended`: the exchanges the loss ended, each with its `seq` and classification;
   - `refused`: the requests the worker refused afterwards on the offline link, naming the loss
     (ADR-294), the same way;
   - each clock's last time before the loss, raw, with its epoch and the agent time it was read;
   - the VCI's serial number, when the API gave one.

   An exchange the loss ended is an exchange entry with the `vci_lost` outcome, or a
   `primitive_ended` with that row. A request refused afterwards is an entry with the `vci_lost`
   outcome refused after the loss.

2. **When it is written.** The first record is written when the first such entry is written. It
   keeps each clock's latest time then. After every further entry the loss ended or caused, the
   record is written again, whole. Each record is synced, so a crash later in the run keeps what
   was recorded. The last record of a loss counts.

3. **What a later run finds.** The opening scan takes the log's last `vci_loss` record, unless a
   `vci_loss_settled` names its `loss`. That loss is settled by the next run whose VCI connects,
   whichever run that is. A run whose VCI never connects leaves it for the next one. A record
   superseded by a later loss is not settled: its exchanges keep their classification.

4. **The settlement.** When the log holds a loss to settle, the runner reads the VCI's clock once
   (`GetTimestamp`) before it writes `vci_connected`. The log then writes `vci_loss_settled`
   right after `vci_connected`, against that reading. The reading is placed on its clock first,
   which opens that clock's first epoch of the run. The comparison follows design 8.11:
   - The gap is measured only when the last time and the reading lie in one agent epoch.
   - The comparison is made only on a D-PDU link, between two times of the VCI's own (`device`)
     clock.
   - The power is kept only when the reading fits a clock that carried on and not one that
     restarted, and the serial numbers match. The exchanges are then `system`.

   Otherwise the loss stays `undetermined`, and the entry says why:
   - a restart (`power_kept` `false`);
   - a reading that fits both or neither, a serial number that does not match or is missing, no
     reading of the device clock on both sides, a J2534 link, an API the job was not told, or a
     gap across agent epochs (`power_kept` `unknown`).

## Consequences

- Every reopen of the log starts a new agent epoch (design 8.11), and one `ngr-agent run` is one
  run. So a later run cannot measure the gap with the agent's monotonic clock, and every loss
  settled today stays `undetermined` with `power_kept` `unknown`. The comparison is implemented
  and tested for a gap one agent epoch measures, and that is all a later change needs to make it
  conclusive. The maintainer chose this scope (2026-10-28). Measuring the gap between two runs
  of one boot (a system-wide monotonic reading in `agent_clock`) is follow-up work. So is the
  rule that a clock shown to have carried on keeps its epoch across the reconnect: no run can
  show that yet.
- On a J2534 link the settlement does not wait for a device-timestamped frame. Design 8.11 lets
  a hardware profile state that the VCI's frame clock is its own and restarts at power-up, but
  the profile has no such field yet, so a J2534 loss settles at the reconnect as `unknown`.
- A loss record and its settlement are small lines. A loss that causes many refusals writes its
  record once per refusal, growing with each one. A run that loses its VCI ends quickly, so few
  are expected.
