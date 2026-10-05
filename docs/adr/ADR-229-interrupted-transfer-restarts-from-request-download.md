# ADR-229: Interrupted Transfers Restart from RequestDownload; Write Procedures Are a Reference Implementation

**Date:** 2026-10-05
**Status:** Accepted
**Affects:** `docs/system-architecture.md` (8.2.5, 16.1), `crates/agent` (write-job journal), `crates/sim-ecu` (flash state), `crates/diag-ir` (resume model)

## Context

A write job that downloads data to an ECU can be interrupted part-way through the transfer: the
agent crashes or the PC restarts, the worker crashes, the VCI is disconnected, or the power
fails. Design 5.6 requires such a job to resume from its journal or to end in a defined state,
and design 8.2.5 set the journal's checkpoints for flash transfers at block granularity. The
open question was where the resumed transfer starts.

ISO 14229-1 (2026 edition) gives the client no standard way to continue a download from a later
block. The TransferData block sequence counter (clause 14.4) is there so that the ECU can tell a
block the client repeated after a lost response from a new block, and answer the repeat without
writing it again. The counter starts over with every RequestDownload, and the download state does
not survive an ECU reset or the end of the programming session. Continuing from the block after
the last confirmed one would need ECU-specific behaviour, such as accepting a RequestDownload for
the remaining address range without a new erase. Whether a given ECU allows that is a vendor
question (design 17, "Items to Confirm Early").

The write and recovery procedures that ship with this software serve as a reference
implementation. In production, the framework user owns the procedures they run and is
responsible for them, in the same way that business screens are theirs (design 9.6, 16.1).

## Decision

1. **Restart from RequestDownload.** When a transfer that has started is interrupted (agent crash
   or restart, worker crash, VCI disconnect, power loss), the resumed job redoes the transfer from
   its erase and RequestDownload, in the order of item 2. It never continues from a later block.
   This applies only where the procedure's interruptibility attribute allows automatic resumption
   (8.10.1): an interruption inside a section marked "recovery required on interruption" (for a
   flash session, every step from `recovery_required_from_step` on, which defaults to the start
   of erase) ends the job in `OnSiteInterventionRequired` instead, as design 5.6 and 8.10.1
   already require. A procedure that allows a restart says so by its attribute; the M1 reference
   procedure against `sim-ecu`, whose bootloader stays intact, is one.
2. **Order of a restart: tear down, then check, then erase.** A crash or a short disconnect does
   not necessarily end the transfer on the ECU: if the agent reconnects before the ECU's session
   timer expires, the ECU can still hold the old download. While it does, a new RequestDownload
   would be refused with a sequence error, an erase could run while the old download is still
   open, and services the checks rely on (such as ReadDataByIdentifier) may not be available. A
   restart therefore runs in this order:
   1. Checks that need no ECU service: the start deadline, the resume limit per stage, the
      interruptibility attribute (item 1) and the supply voltage read through the VCI.
   2. Teardown. The agent ends any download the ECU may still hold with an ECUReset (ISO 14229-1
      clause 9.3), which takes the ECU out of its non-default session. If the ECU refuses the
      reset or does not answer, the agent stops sending TesterPresent, waits for the session
      timeout the procedure declares plus a margin (the framework has no built-in default, since
      the value is ECU-specific), and then confirms that the ECU is back in its default session,
      for example by reading the active-session data identifier F186 (ISO 14229-1 Annex C). If
      it cannot confirm that, the job ends in `OnSiteInterventionRequired`. The reset is sent
      before the VIN is verified; it changes no memory, and nothing destructive happens before
      step 3.
   3. Service-dependent checks in the default session: VIN verification and the ECU state check
      of design 8.2.5. A VIN mismatch aborts the job. The state check includes reading back the
      ECU software version: if it shows that the intended image is already installed (the
      transfer and RequestTransferExit completed, but the response or the journal update was
      lost), the job skips the restart and continues with read-back verification.
   4. Re-entry: the programming session and security access again, then erase and RequestDownload.
3. **Block checkpoints are progress, not a resume origin.** The journal still records each
   confirmed block, for progress display and for the checkpoint summary used for handover to
   another device (8.2.5). Repeating a block after a lost response remains the block sequence
   counter's job inside one transfer.
4. **Split of responsibility.** The framework provides the mechanisms: the journal, the state
   check before resuming, the idempotency and interruptibility attributes (8.2.5, 8.10.1) and the
   guarantee that a write job ends in a state defined by design 5.6. The bundled write and
   recovery procedures are a reference implementation that uses only standard services. Recovery
   strategies that depend on a particular ECU, such as continuing from a later address, are the
   framework user's to write as their own procedures, on top of the same mechanisms.

## Consequences

- The reference implementation works with any ECU that implements the standard download
  services. `sim-ecu` only needs the standard behaviour: the download state survives a client
  reconnection within the session timer, is lost on reset or session end, and a repeated block
  with the previous counter value is accepted without being written again. The recovery tests
  cover a fast reconnection (within the session timer) as well as a restart after it expired.
- The IR has no field yet for the session timeout a procedure declares (item 2); it is added
  together with the write-job journal.
- With the schema default, a flash session never restarts automatically once erase has begun;
  the restart rule takes effect only for procedures whose authors declare that the ECU can be
  reprogrammed again after such an interruption.
- An interrupted transfer of a large image is resent in full, which costs time. The resume limit
  per stage (8.2.5) still applies, so repeated failures end in on-site intervention rather than
  an endless loop.
- A framework user who adds an ECU-specific continuation is responsible for its correctness
  against that ECU. The framework does not check it beyond the attributes and the state check.
- Design 16.1 lists production write and recovery procedures as the framework user's
  responsibility.
