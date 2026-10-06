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
   procedure against `sim-ecu`, whose bootloader stays intact, is one. For a flash session that
   means declaring that no step requires on-site intervention, which the IR cannot express yet
   (see Consequences).
2. **Order of a restart: tear down, then check, then erase.** A crash or a short disconnect does
   not necessarily end the transfer on the ECU: if the agent reconnects before the ECU's session
   timer expires, the ECU can still hold the old download. While it does, a new RequestDownload
   would be refused with a sequence error, an erase could run while the old download is still
   open, and services the checks rely on (such as ReadDataByIdentifier) may not be available. A
   restart therefore runs in this order:
   1. Checks that need no ECU service. After an agent crash or a loss of the device's own
      power the restart is a new run on this device, and the OS-level locks of the interrupted run were released with it,
      so before anything goes through the VCI the agent takes the per-VCI lock and the device's
      single reprogramming slot again (design 8.8 and 8.8.1), and it promotes the per-VCI lock
      to the per-vehicle lock as soon as a VIN read first matches the job's VIN, in step 2 or,
      when step 2 could not read it, in step 3, and before anything after that match; while
      another job holds either, the restart waits, and it expires at the start deadline like any
      job. After a worker crash, a VCI disconnect or a loss of the vehicle's or ECU's supply
      alone, the agent process survives and its job still holds these guards (and the per-vehicle lock, if it already had it), so the
      restart keeps them rather than taking them again, which would leave it waiting on its
      own locks. Then it checks the start deadline, the resume
      limit per stage, the interruptibility attribute (item 1) and the supply voltage read
      through the VCI. If the
      limit allows another resume, the agent increments the stage's resume count and commits it
      to the journal, and also reserves the attempt on the server by incrementing the server's
      per-stage count atomically and checking it against the limit, all before it sends anything
      to the ECU. The agent journals a key for the attempt before it reserves, and the server
      reservation is idempotent on that key, so a reservation whose response was lost is retried
      under the same key and never consumes a second attempt. Once the server confirms the
      reservation, the agent journals that confirmation before its first request to the ECU; on a
      later restart a key whose confirmation is journaled is retired and a new attempt gets a new
      key, so a crash after recovery traffic began always consumes an attempt, while a key without
      a journaled confirmation is retried as above. The journal commit keeps a crash
      during recovery from reloading the old count;
      the server reservation keeps a handover from starting on a count that misses attempts this
      device made but never published. An agent deployed without a server (the standalone
      setup of milestone M1, before the server exists) has no handover either, so there the
      journal count alone enforces the limit and no reservation is made. If a configured server
      cannot be reached, the agent waits for it
      until the start deadline; once that passes, the job ends as expired, like any job past
      its start deadline (design 8.2.5). Recovery is
      rare and this happens before anything is sent to the ECU, so the network dependence costs
      only availability, unlike the RequestTransferExit acknowledgement rejected below.
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
      stopped, ignition state, and the supply voltage and external power supply, since a reset
      can leave a partially programmed ECU depending on stable power) as far as it can without changing anything on the vehicle. Only
      when they all hold does it end any download the ECU may still hold with an ECUReset
      (ISO 14229-1 clause 9.3), which takes the ECU out of its non-default session. One case is
      excluded: when the journal shows that RequestTransferExit had been sent but does not record
      the procedure's post-transfer steps (such as CheckMemory) as complete, the agent sends no
      ECUReset, because a reset at that point could activate an image the procedure has not yet
      validated. When the journal records the post-transfer steps (including the procedure's
      own ECUReset) as complete, no download is left to tear down and the agent sends no reset,
      so a freshly initialized ECU is not reset again before verification. If the default-session
      confirmation below succeeds, it continues from there. If it does not, because the ECU
      reports a non-default session or because it does not answer F186 or refuses it (an ECU
      can reject the identifier outside its default session, for example when the procedure's
      steps end without an ECUReset), the agent tears down passively and then repeats the
      confirmation once, so only a failure after that teardown ends in on-site intervention. So that a crash between sending RequestTransferExit and recording its response
      cannot hide this case, the agent commits an intent marker for RequestTransferExit to the
      journal before it transmits the request (write-ahead); a journal without that marker
      proves the request was never sent. The marker is part of the checkpoint summary used for
      handover, like the recorded identity below.

      In every other case it tears down passively: when the ECU does not answer the VIN read or
      answers it with a negative response (for example because it is still inside the old
      download and does not allow the service there), when the hardware identity read gets no
      answer, a negative response or a value that differs from the journal's, when a safety
      precondition fails or cannot be established, in the post-transfer case just excluded,
      when the ECU refuses the reset, and when the reset gets no response so that its outcome is
      unknown. Passive teardown means the agent stops sending TesterPresent and
      waits for the session timeout the procedure declares plus the margin it also declares
      (the framework has no built-in default for either, since both are ECU-specific).

      After either kind of teardown, or directly in the completed case above, the agent confirms that the ECU is back in its default
      session by reading the active-session data identifier F186 (ISO 14229-1 Annex C). This is
      the reference implementation's only confirmation query, so the IR needs no mapping for it;
      an ECU that does not answer it, or answers it with a session other than the default one,
      is not confirmed. An ECU that has just accepted a reset may not answer while it restarts, so
      in every case, after either kind of teardown and on the completed path that sends no reset,
      the agent first waits the startup time the procedure declares, after the session timeout
      when the teardown was passive, and then retries the read within a bounded window. This
      applies to a passive teardown and to the completed path too, because a reset may have been
      accepted, or recorded as complete just before the crash, while the ECU is still restarting,
      whether it was the teardown reset
      or an ECUReset among the procedure's own post-transfer steps, so the journal cannot rule
      out that the ECU is restarting (the reset response's power-down time,
      when the ECU reports one, extends the wait). If it still cannot confirm the default
      session, the job ends in `OnSiteInterventionRequired`. Step 3
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
      post-transfer steps again in the procedure's order. The reference implementation has no
      validation-only recovery for this case; the IR has no way to declare one, and an ECU that
      needs it is served by the framework user's own procedure (item 4). If the software version is the
      one recorded before the erase, or the ECU conclusively reports that it has none (a
      response the procedure declares to mean that no valid application is present, such as a
      specific negative response code), the transfer still needs to be redone and the job goes
      on to step 4. No answer, any other negative response, or a positive response whose version
      cannot be decoded (for example truncated or malformed metadata left by the interrupted
      write) proves nothing, since the
      response of an ECU holding an unexpected version may simply have been lost: the agent
      retries the read within the procedure's retry limit, and if it stays inconclusive the job
      ends in `OnSiteInterventionRequired`. The journal's record of the post-transfer steps decides, not the version
      alone: when a job reflashes the version already installed, the pre-erase and intended
      versions are the same, and the job skips the restart only if those steps are recorded as
      complete; otherwise it redoes the transfer. Any other decoded version means something other than this job changed the ECU, and
      the job ends in `OnSiteInterventionRequired`.
   4. Re-entry: before anything else in this step, every execution precondition of design 8.9
      that the procedure declares is validated (for a flash session: the voltage range, ignition
      on, engine off, vehicle stopped, and the power supply check), since conditions may have
      changed while the agent was down and the `Interrupted -> Writing` transition does not pass
      through pre-validation again. The one exception is the current-software-version match of
      design 8.9.1: during a restart it is replaced by the step 3 rules (the pre-erase version,
      the intended version or the declared no-application response are acceptable), because an erased
      application may no longer report a version; the VIN and hardware part number match of
      8.9.1 still applies, as established in step 3. If any precondition fails, the job ends in `OnSiteInterventionRequired`
      before the programming session or any side-effecting setup step is replayed. Then the
      procedure's own steps that lead up to the erase are replayed, from the recovery entry
      boundary the flash session declares (Consequences) up to its erase boundary, with their guards (programming session, security access, and any
      pre-programming steps the procedure defines, such as CommunicationControl,
      ControlDTCSetting or prerequisite routines; design 8.9), because the teardown discards the
      state they established. Immediately before the erase the conditions that can change
      during the replay (voltage, external supply, ignition, engine and vehicle-speed states)
      are checked a second time, since the replay takes time. The VIN and hardware identity
      established in step 3 are not read again, because the job holds the per-vehicle lock
      and the ECU is now in its programming session, where those reads may not be available.
      Each state checked here must come from a source the procedure declares usable in the
      programming session (a service the ECU answers in that session, or a runtime input from
      the VCI or the agent), while the full check at the start of step 4 runs in the default
      session and needs a source usable there; the procedure may declare one source for both or
      a separate one for each session. A procedure that allows a restart without a usable source
      for every checked state in both sessions is rejected when it is loaded, not discovered
      during recovery. If any
      check fails, the job does not erase and ends in `OnSiteInterventionRequired`, reporting
      the failed condition. Then erase and RequestDownload.
   A handover to another device (8.2.5) follows the same order. The receiving agent holds
   none of the failed device's guards, so like a restart after an agent crash it takes its own
   per-VCI lock and its device's reprogramming slot before anything goes through its VCI,
   waiting while another job holds them, and promotes to the per-vehicle lock immediately after
   the first VIN read that matches, before any further ECU traffic. The receiving agent has no
   access to the failed device's journal, so the checkpoint summary sent to the server carries
   the recorded ECU hardware part number, the pre-erase software version, the RequestTransferExit
   intent marker, the post-transfer progress and the number of resumes already made per stage
   (so the resume limit of step 1 holds across devices; because every recovery attempt,
   including the failed device's own, is reserved on the server before anything is sent to the
   ECU, the server count includes them all, and the receiving agent increments that count
   atomically on the server, and checks it against the limit, before it starts a handover
   recovery, rather than trusting the count it read) along with the VIN, and
   the receiving agent compares against those values. The summary can lag behind the failed
   device's journal (the device may fail after committing the marker locally and sending the
   request, but before the updated summary reaches the server), so on a handover an absent
   marker proves nothing: the receiving agent never sends the step 2 ECUReset and always tears
   down passively, unless the summary records the post-transfer steps as complete, in which
   case it goes straight to the default-session confirmation as on the same device. Requiring
   a server acknowledgement before each RequestTransferExit was rejected, because it would make
   the transfer depend on the network at that point. The same lag can leave the summary
   without the recorded hardware part number or the pre-erase software version (for example
   after an offline write, or a crash before the upload). The receiving agent then has nothing
   to compare the ECU's identity or version against, so a handover whose summary lacks either
   value ends in `OnSiteInterventionRequired` before anything is sent to the ECU.
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

- The reference implementation's automatic recovery works with an ECU that implements the
  standard download services and, in its default session, answers ReadDataByIdentifier for
  the VIN, its hardware part number, its software version and the active session (F186)
  (ECUReset is optional, since passive teardown covers its absence), and whose software
  version read, while an interrupted transfer is pending, gives either the pre-erase version,
  the intended version or the no-application response the procedure declares. An ECU whose
  version read exposes partially written metadata instead gives a value that is neither, and
  the job ends in `OnSiteInterventionRequired`, because that value cannot be told apart from a
  change made by something else. An ECU that lacks any of these can still be programmed, but an interrupted
  transfer on it ends in `OnSiteInterventionRequired`. `sim-ecu` provides all of them and
  otherwise only needs the standard behaviour: the download state survives a client
  reconnection within the session timer, is lost on reset or session end, and a repeated block
  with the previous counter value is accepted without being written again. The recovery tests
  cover a fast reconnection (within the session timer) as well as a restart after it expired.
- The IR has no fields yet for the session timeout a procedure declares and the margin added to it before a passive teardown is confirmed, the response that
  means no valid application is present, the retry limit for the recovery version read, the ECU startup time after a reset and the length of the window in which the session confirmation is retried, the
  resume limit of each stage (the stage it applies to and its maximum; `VmState` today keeps
  a single `resume_count` and no section or flash session declares a maximum), or a
  required external power supply (item 2; `FlashSession` today declares only the voltage
  range, ignition, engine-off and vehicle-stopped preconditions), nor a mapping that names
  which of the procedure's services and response fields yield the VIN, the hardware part
  number, the software version and the engine, vehicle-speed and ignition states that the
  declared safety preconditions test, each with the value or range that satisfies it (these
  identifiers can be OEM-specific; a state that is not read from the ECU comes from a runtime
  input instead, as the external supply does, and the mapping gives each state a source for the default session and one for the
  programming session, which may be the same, for the two checks of item 2 step 4), nor the recovery boundaries of each flash
  session: where the replayable steps leading up to the erase begin, where the erase begins,
  where RequestTransferExit is sent and where the procedure's post-transfer steps end, so the
  runtime replays exactly the pre-erase range, checks the preconditions again at the erase
  boundary, and journals the post-transfer steps as complete only when execution reaches
  their end boundary (today `Section` has only start and end positions and `FlashSession`'s steps
  are not linked to the bytecode), nor a way for a flash session to declare that no step
  requires on-site intervention (today `recovery_required_from_step` is a step number that
  defaults to the start of erase and has no value for "none", so the M1 reference procedure of
  item 1 cannot allow a restart after erase); all eleven are
  added together with the write-job journal, along with the runtime input that reports whether the supply is
  connected. Until then a procedure cannot require the supply, and the voltage range is the
  only power check a restart can make. A declared safety precondition whose source is not
  mapped cannot be established, so the restart treats it as failed: no ECUReset is sent and
  the job ends in `OnSiteInterventionRequired` before any erase. Without a declared
  limit the read is not retried. Until a procedure declares the latter, no response to the software
  version read counts as conclusive and an interruption after erase ends in
  `OnSiteInterventionRequired`.
- The server has no operation yet for reserving a recovery attempt. It needs one that
  atomically increments the per-stage resume count, checks it against the limit and is
  idempotent on the attempt key, backed by durable storage of the per-stage counts and of the
  attempt keys with uniqueness on job, stage and key. It is added with the server side of
  recovery and handover, once the server's control channel exists, not with the agent's
  write-job journal: an agent without a configured server recovers on its journal count
  alone, so the journal does not depend on it.
- The server also records which device owns the job, and the reservation is refused to any
  other device. The per-VIN server lock is only advisory (design 8.8), and a counter alone
  does not stop a failed device that comes back from recovering the same ECU while another
  device performs the handover. Ownership moves to the receiving device only when the
  operator confirms on the server that the failed device is disconnected from the vehicle (or
  powered off); from then on the failed device's reservations are refused, so it sends nothing
  further to the ECU. The transfer also starts a new ownership generation. The generation is
  part of the signed job instruction and is carried by every job-scoped message the agent
  sends (job state, progress, HMI requests, checkpoint summaries and reservations), and the server applies one only when both the
  device and the generation match the current ones exactly, so a message queued under an
  earlier generation is refused even if ownership has since returned to the same device; anything the former owner sends later, such as messages
  queued while it was offline, is kept only as audit data (with its uploaded journal) and
  never overwrites the receiver's state or summary. The agent's duplicate-command check
  (design 5.3) keys on the job ID and the ownership generation together, so when ownership
  returns to a device that ran the job before, the newly signed instruction under the higher
  generation is accepted as a new command, while a resent instruction of the same or an
  earlier generation is still ignored as a duplicate. Fencing by operator confirmation was chosen over a time-limited lease
  renewed during the write, because a lease would make every write on a server-connected
  device, the first run included, depend on the network for its whole duration and rule out
  starting a write offline (design 5.7). An agent without a configured server has no handover
  and no ownership record. The operator's confirmation is an operation of the server's Web
  API, limited to operators authorized for the job and written to the audit log; it is added
  together with the reservation operation. Because a signed job instruction names the agent
  and the VCI it runs on, the transfer also issues a new signed instruction bound to the
  receiving agent and VCI, after the same signing, approval and compatibility checks as a new
  job; the receiving agent never runs the failed device's instruction.
- A false confirmation is not prevented. An operator can confirm that the failed device is
  disconnected while it is still writing, and both devices may then access the same ECU.
  Neither a minimum time without contact from the failed device nor a second approval is
  required before a handover. Simultaneous access that gets past the framework's own guards
  (the device locks, the job ownership on the server and the interference detection of
  design 8.8) is left to the operators' procedures and to defences on the vehicle side. Those
  differ by transport: DoIP lets an ECU accept one active tester at a time, while UDS on CAN
  generally has no such arbitration, so on CAN vehicles the procedure is the only defence.
  This follows design 8.8, which already holds that complete exclusivity is impossible
  because third-party tools can always be connected, and the premise of item 4 that
  production procedures are the framework user's responsibility.
- Misuse is made detectable instead, by records that only system administrators can audit.
  The server keeps them append-only, and operators cannot change them. The handover record
  holds the confirming operator, the time, both devices, and the failed device's last
  contact with the server at the moment of confirmation, so contact after the confirmation
  shows that it was false. A failed device that reconnects uploads its journal, and the
  journal is protected against tampering so that what it uploads can be trusted. Every VIN
  and ECU hardware identity that either device reads is recorded, including mismatches and
  aborted reads, and the server alerts the administrators when one job reports more than one
  VIN or when a job aborts on a VIN mismatch after a handover.
- Because every signed instruction, including the one reissued at a handover, is bound to one
  VIN, a false confirmation cannot be used to reprogram a second vehicle with one approval:
  the receiving device stops at the VIN check, the failed device re-reads the VIN on every
  recovery, and moving a VCI to another vehicle during a transfer meets an ECU that holds no
  download and is stopped by the interference detection. Two cases remain outside the
  system: hardware placed between the VCI and the vehicle that reports a false VIN (a
  physical attack, not addressed), and an image copied off the device and written with a
  third-party tool, which protection of artifacts at rest on the device would have to
  address.
- Because every recovery on an agent with a configured server reserves its attempt there
  first, a write job interrupted
  while the device is offline (design 5.7 lets jobs start offline) does not restart until the
  server is reachable again, and expires if its start deadline passes first.
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
