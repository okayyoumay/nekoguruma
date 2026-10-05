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
   2. Identity, safety, then teardown. The agent never sends an ECUReset to an ECU it has not
      identified, because the VCI may now be connected to a different vehicle, nor to a vehicle
      that may be running. It first tries to read the VIN, which changes nothing on the ECU. Only
      a VIN that is read and decoded and differs from the job's VIN counts as a mismatch, and the
      job aborts. A matching VIN identifies the vehicle, not the ECU the VCI addresses, so the
      agent next reads that ECU's hardware identity, which also changes nothing: its ECU hardware
      part number (design 8.9.1 variant identification), recorded in the journal before the
      erase. The software version is not part of this identity, because an ECU whose application
      was erased may stop reporting it while its bootloader still answers; it is used only by the
      state check in step 3. If the VIN and the hardware identity match, the agent checks the
      safety preconditions of design 8.9 that the procedure declares (engine off, vehicle
      stopped, ignition state) as far as it can without changing anything on the vehicle. Only
      when they all hold does it end any download the ECU may still hold with an ECUReset
      (ISO 14229-1 clause 9.3), which takes the ECU out of its non-default session. One case is
      excluded: when the journal shows that RequestTransferExit had been sent but does not record
      the procedure's post-transfer steps (such as CheckMemory) as complete, the agent sends no
      ECUReset, because a reset at that point could activate an image the procedure has not yet
      validated.

      In every other case it tears down passively: when the ECU does not answer the VIN read or
      answers it with a negative response (for example because it is still inside the old
      download and does not allow the service there), when the hardware identity read gets no
      answer, a negative response or a value that differs from the journal's, when a safety
      precondition fails or cannot be established, in the post-transfer case just excluded,
      when the ECU refuses the reset, and when the reset gets no response so that its outcome is
      unknown. Passive teardown means the agent stops sending TesterPresent and
      waits for the session timeout the procedure declares plus a margin (the framework has no
      built-in default, since the value is ECU-specific).

      After either kind of teardown the agent confirms that the ECU is back in its default
      session, for example by reading the active-session data identifier F186 (ISO 14229-1
      Annex C). If it cannot confirm that, the job ends in `OnSiteInterventionRequired`. Step 3
      never starts before this confirmation.
   3. Service-dependent checks in the default session. The VIN must be read, decoded and equal
      to the job's VIN before anything else happens; this is required again even when step 2
      already matched it. A decoded VIN that differs aborts the job, and any other outcome (no
      answer, a negative response, an undecodable value) ends it in `OnSiteInterventionRequired`.
      The hardware identity of step 2 is likewise required again here and is authoritative: a
      decoded hardware part number that differs from the journal's aborts the job, and any other
      failure to establish it (no answer, a negative response, an undecodable value) ends it in
      `OnSiteInterventionRequired`. Then the ECU state check of design 8.2.5, which includes
      reading back the ECU software version. If it shows that the intended image is already
      installed and the journal records the procedure's post-transfer steps (such as CheckMemory
      and the procedure's own ECUReset) as complete, so that only a response or the journal
      update was lost, the job skips the restart and continues with read-back verification. If
      it shows the intended version but those steps are not recorded as complete, the image is
      not treated as validated and the job goes on to step 4, whose full transfer runs the
      post-transfer steps again in the procedure's order; a procedure may instead declare its
      own post-transfer recovery sequence for this case (item 4). If the software version is the
      one recorded before the erase, or the ECU reports none (no answer or a negative response,
      as when the application is invalid), the transfer still needs to be redone and the job goes
      on to step 4. The journal's record of the post-transfer steps decides, not the version
      alone: when a job reflashes the version already installed, the pre-erase and intended
      versions are the same, and the job skips the restart only if those steps are recorded as
      complete; otherwise it redoes the transfer. Any other decoded version means something other than this job changed the ECU, and
      the job ends in `OnSiteInterventionRequired`.
   4. Re-entry: the procedure's own steps that lead up to the erase are replayed from the start
      of the flash session, with their guards (programming session, security access, and any
      pre-programming steps the procedure defines, such as CommunicationControl,
      ControlDTCSetting or prerequisite routines; design 8.9), because the teardown discards the
      state they established. Immediately before the erase, every execution precondition of
      design 8.9 that the procedure declares is checked again (for a flash session: the voltage
      range, ignition on, engine off, vehicle stopped, and the power supply check), since
      conditions may have changed while the agent was down and the `Interrupted -> Writing`
      transition does not pass through pre-validation again. If any fails, the job does not
      erase and ends in `OnSiteInterventionRequired`, reporting the failed condition. Then erase
      and RequestDownload.
   A handover to another device (8.2.5) follows the same order. The receiving agent has no
   access to the failed device's journal, so the checkpoint summary sent to the server carries
   the recorded ECU hardware part number and pre-erase software version along with the VIN, and
   the receiving agent compares against those values.
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
