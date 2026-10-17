# ADR-325: Procedures That Address Several ECUs

**Date:** 2026-10-17
**Status:** Accepted
**Affects:** `diag-ir` (`Program`, `Op`, `VmState`, `DiagHost`, the restart declaration),
`diag-frontend` (procedure targets, `diag.ecu`, access programs, lowering per combination of
variants), `agent` (`Job`, `check_job`, policy, one link per target, keep-alive, run and restart
order), the workers' tester-present ComParams, `sim-ecu` (a gateway role), design 8.2.4, 8.2.5,
8.2.6, 5.6, 8.10, 8.10.1; ADR-229 item 2, ADR-235 item 8, ADR-245, ADR-247 item 1, ADR-257, ADR-263 item 4,
ADR-264 item 4, ADR-280, ADR-307 items 1 and 5, ADR-311 item 1, ADR-316, ADR-318 items 1, 3 and 4

## Context

A procedure is lowered against one variant's declaration part and shipped in that variant's
document (ADR-307, ADR-311); a job holds that one program and one document (ADR-318), and the
agent opens one link with one request and response identifier pair before the first
instruction. No instruction names an ECU, so a procedure can address exactly one ECU.

Many jobs need more than one in a single procedure:

- Functions the vehicle protects behind a security gateway: the gateway ECU is put into an
  extended session and unlocked with security access, and must stay in that session while the
  tester works on the ECU behind it (a reprogramming, an input-output control, a routine). The
  ECUs behind it may not even answer identification requests until it is unlocked.
- Reprogramming itself, which in the usual sequence tells the other ECUs to stop their normal
  communication and their DTC recording before the download and to resume both afterwards
  (CommunicationControl and ControlDTCSetting).
- Immobilizer and key learning, which runs between the engine, body and immobilizer ECUs.
- Active tests judged by another ECU's values (driving a pump on one ECU while reading wheel
  speeds from another), sensor calibrations that span ECUs, and vehicle-wide DTC reads and
  clears.

Two shapes were weighed. In the first, a procedure stays bound to one ECU and declares a
prerequisite (for example a gateway unlock) that the agent runs on another link before it and
keeps alive. It changes neither the IR nor the VM, but covers only the gateway case: none of
the other jobs above fits a prerequisite that runs once at the start. In the second, chosen
here, a procedure names several targets and the IR switches between them. The gateway case
still needs its unlock to run before anything behind the gateway is identified, so the first
shape survives inside the second as the access program of item 5.

## Decision

1. **A procedure names its targets.** A procedure declares the ECUs it addresses as targets,
   each a role name the procedure chooses (`gateway`, `engine`) bound to an ECU definition that
   the conversion finds in the same package by the definition's identifier. The ECU of the
   definition the procedure belongs to is the primary target, which a procedure that declares
   no other target addresses alone, as before. A target is reached physically, by the
   addressing of its definition's protocol. Targets addressed functionally (one request that
   several ECUs answer) are not part of this decision; they are decided with the functional
   requests of the generic OBD package, and the target model leaves room for them. The
   conversion refuses more than eight targets per procedure.
2. **Starlark addresses a target through a handle.** `diag.ecu(role)` returns the handle of a
   declared target; the role must be a constant, so every handle is known at conversion. The
   diagnostic calls are available on the handle (`gw.security_access(level)`,
   `eng.request(...)`). Handles follow ADR-300's rules for function values: bound to a name or
   passed as an argument, never chosen by a conditional expression or stored in a container, and
   a function that receives a handle is specialized per handle. The module-level calls keep addressing the
   primary target, so existing procedures do not change. Each call is lowered against its
   target's declaration part.
3. **The IR selects the target.** `Program` lists its targets, the primary first. A
   `SelectTarget` instruction, appended after the existing ones (ADR-233), makes one of them
   current, and the VM tells the host on every change and when it starts from a restored
   state. The instructions that address an ECU act on the current target, its link and its
   declarations: `ServiceRequest`, `SendReceive`, `ReadDtc`, `RoutineControl`,
   `SecurityAccess` (whose key request to the server names the current target's ECU),
   `FlashTransfer`, `MonitorCapture` and `DecodeField` (extending ADR-316's decoding to the
   current target's declaration part); `Wait`, `HmiRequest`, `RecordInput`,
   `Log` and `Fail` address none. The current target is part of `VmState`, so the journal
   records it with the rest of the state and a restored state resumes on the right target. A
   program starts on the primary target. The lowering emits `SelectTarget` before every
   instruction that addresses an ECU whose target is not certainly current on every path that
   reaches it, so the existing instruction encodings stay as they are.
4. **The restart declaration names its target.** The identity sources, the precondition sources
   and the flash recovery plans of the procedure part (ADR-245) each name the target they refer
   to. The VIN source may be on any target; the hardware identity and every flash plan are on
   the reprogramming target (item 9). `Program::validate`, which every load runs, refuses flash
   plans on two targets, a hardware identity off the reprogramming target, a download service
   addressed to any other target, and any instruction between a plan's erase and its
   post-transfer end that addresses another target.
5. **Access programs.** A target whose session and unlock other targets depend on (a gateway)
   can be declared an access target, with an access program: steps that only change that
   target's session and security state and read from it (session control to the default or an
   extended session only, security access, reads). The declaration also gives the target's
   session timeout and tester-present period (item 11). Several access targets (a gateway behind
   a gateway) run their access programs in declaration order. The access program is lowered on
   its own, once per variant of the access target, and travels with the job; the conversion and
   `check_job` refuse any other service in it. A first run goes in this order: for each access
   target in declaration order, identify it (by reads, which a locked gateway answers) and run
   its access program at once, so that an inner gateway is reached through an unlocked outer
   one; then read
   the VIN and take the per-vehicle lock when it matches (8.8), identify the other targets, and
   pick the combination (item 7). A restart (8.2.5) runs the access programs after the locks and
   the server reservation and before the VIN read of ADR-229 item 2 step 2. Every run also runs
   them again after any reopening of the links (a worker crash or VCI disconnect that the job
   survives) before the program continues. Their session change and unlock reach an ECU before
   the vehicle is identified and before the 8.9 safety preconditions are read, which ADR-229's
   order otherwise avoids; this is accepted because a correct unlock changes no stored state (a
   wrong key counts against the ECU's failed-attempt limit, as any security access does) and
   actuates nothing, and the sessions lapse with their timeout. When the VIN then differs, the
   agent stops the keep-alive, closes the links and lets those sessions lapse; on a first run
   the job ends in the 8.9.1 pre-check with nothing written, and on a restart as ADR-286
   describes. When the VIN matches and the per-vehicle lock must be waited for because another
   job holds it, the agent stops every keep-alive before it waits and sends nothing while it
   waits, as ADR-263 requires. Once it holds the lock it again identifies each access target and
   runs its access program, in the same order, since the job that held the lock may have changed
   them, before anything else is sent. A lock failure leaves only the identification
   reads, the access programs and the VIN read sent (amending ADR-263 item 4), and their sessions
   lapse. The main program may still select the access target for anything else.
6. **Lowered once per combination of variants.** The main program is lowered for each
   combination of one variant per target, extending ADR-311's per-variant lowering, and must
   lower for every combination; the scoping of a procedure to some variants that ADR-311 item 2
   leaves open applies to targets as well. Each combination's program goes in its own document
   for the primary variant (amending ADR-307 item 1's one document per variant and procedure),
   which names, for each other target, the content hash (design 8.2.6) of that variant's
   declaration-only document; the conversion emits a declaration-only document for every
   variant of a definition another procedure targets. The document's digest covers every
   definition its program was lowered against (amending ADR-307 item 5). The conversion refuses
   a procedure whose combinations exceed a bound set with the implementation, so the package
   cannot grow without limit.
7. **A job holds the documents of every combination it may meet.** Since the variants behind an
   access target are known only after its access program has run, a job names, by content hash,
   its access programs, the declaration-only documents of every candidate variant of every
   target, and every combination's document (amending ADR-318 item 1), and the agent holds all
   of them before anything is sent; the bound of item 6 keeps their number finite.
   `check_job` validates all of them then: it refuses a combination document whose named hashes
   do not match the documents the job holds, checks each combination's decodes against its
   targets' declarations, refuses flash plans on more than one target, and refuses an access
   program with any service item 5 does not allow. The agent identifies each target's variant as
   the primary's is identified (8.9.1, ADR-315), through those declaration-only documents, and
   the combination its variants pick is the program that runs. A writing job creates its journal
   before anything is sent, as now (ADR-252, ADR-293), and commits a combination record holding
   the chosen document's hash after identification and before the chosen program's first
   instruction. A restart of a journal with that record uses that document (amending ADR-318
   item 4's journal that records no document; the caller still supplies the documents) and,
   once the VIN matches and the per-vehicle lock is held, identifies every target again; a
   variant that differs from the recorded combination ends the restart in on-site intervention.
   A journal without the record holds no step of the program, since only access programs and
   identification reads ran, so its job starts again as a first run does. The host and the
   restart carry these documents in place of the single one (amending ADR-318 item 3).
8. **One link per target, under the same guards.** Before anything is sent the agent opens a
   link to each target, all on the job's VCI, under the job's per-VCI lock and, for a writing
   job, the reprogramming slot (8.8, ADR-257); the per-vehicle lock is still taken when the VIN
   first matches. Targets on buses the VCI cannot open at the same time are refused before
   anything is sent.
9. **At most one target is reprogrammed.** A job's flash sessions and recovery plans all belong
   to one target, the reprogramming target, whose identity the journal records and the restart
   checks (ADR-229). Other targets may receive any request the job's policy allows (security
   access, writes, input-output control, routines, resets) except the download services
   (RequestDownload, TransferData, RequestTransferExit and the related upload and file
   transfer services), which the conversion refuses there. An erase is a routine and cannot be
   told apart from other routines on those targets; a procedure that erases another ECU's memory
   without a flash plan does so unprotected, as any routine does today.
10. **The policy has an access class.** The services an access program may send do not make a
    job a writing job: a read-only job with access programs needs no reprogramming slot (ADR-257),
    and the read-only policy allows session control (default and extended sessions) and security
    access inside access programs only, widening the read-only ceiling of ADR-235 item 8 and
    ADR-247 item 1 there. Security access still needs the server's key (ADR-259), so an access
    program that unlocks cannot run on a standalone agent or offline.
11. **The worker keeps access targets alive.** For each access target, from the start of its
    access program to the end of the job (so the session survives the server round trip of its
    own security access), the agent sets ISO 22900-2 (2022) Annex B's tester-present
    ComParams on that target's link: periodic sending (not the idle mode that fires only when the
    channel is quiet, which traffic to another target on the same channel would starve), a
    TesterPresent message the agent builds itself with the suppress-positive-response bit clear,
    and the expected positive and negative responses set, so that the module reports an answer
    that stops coming. The worker sends it while the VM runs and while it waits on the server, an
    HMI response or a `Wait`, at the period the target's declaration gives; it starts after
    communication is started on that link. Because the
    agent fixes the message, the procedure cannot send anything through the keep-alive. The same
    settings can keep a single target's own session alive, which is decided separately.
12. **A lost access target ends the job by the section rules.** The worker reports a failed
    tester-present as the tester-present error event of ISO 22900-2 (2022) Annex D, which the
    J2534 worker does not produce yet; the module discards the expected answers, so the agent sees
    only these events. The agent counts an access target as lost when error events keep arriving
    for every tester-present period across that target's declared session timeout, latches the
    first such loss, and adapts design 8.10.1's rule for an expired authentication, without the
    renewal: outside any section and in an interruptible section the job ends at the next
    instruction boundary as failed, terminal as ADR-280's declared failure and journaled the same
    way, with its own failure code (a new edge from Writing to Failed in design 5.6); in an
    uninterruptible section it continues to the section's end and then ends the same way; in a
    section marked recovery required it ends in on-site intervention. In a restart's steps
    before the erase it ends as failed when the journal holds no transfer-start marker, and in
    on-site intervention, which a later resume may retry (ADR-290), when it does, since an earlier
    erase may have left the ECU without a valid application. The agent does not run the
    access program again by itself. Keep-alive answers from targets that are not access targets
    are not watched: a request to such a target that fails is handled as any failed request is.
13. **The reprogramming target's keep-alive stops at its flash plan.** When the reprogramming
    target is itself an access target (a gateway being reprogrammed), its keep-alive is watched
    only until its flash plan's entry and stops there, since the erase, the reset and the restart
    of the ECU silence it; the plan's own steps handle that ECU from then on. After the plan's
    post-transfer end the agent runs that target's access program again, and keeps it alive
    again, before any later instruction addresses another target. The restart's
    teardown (ADR-264) likewise stops the reprogramming target's keep-alive before the ECUReset or
    the passive wait, and keeps the other access targets alive.

## Consequences

- `IR_SCHEMA_VERSION` changes with the target list, `SelectTarget` and the targets of the restart
  declaration, and the journal's state with the current target; nothing stored outside the
  repository needs a migration (design 8.2.6).
- The number of documents grows with the product of the targets' variant counts. The target limit
  and the bound of item 6 keep it finite; a definition that needs more must split its procedure
  or scope it to some variants once that exists.
- A gateway unlock is written once and shared between procedures through the shared-file `load`
  of the Starlark subset, which does not exist yet; until it does, each procedure carries its own
  copy.
- Functional targets, needed for the CommunicationControl and ControlDTCSetting broadcast of a
  reprogramming, remain open (item 1). Until they exist, a procedure can send those requests to
  each ECU through a physical target, which multiplies the combinations of item 6.
- Other targets' state after a crash (communication disabled, DTC recording off) returns to
  normal when their sessions time out; the restart's wait covers only the reprogramming target's
  session, so a procedure that changes another target's state must restore it explicitly.
- Whether to keep a single target's own session alive the same way, and with which loss rule,
  is the open TesterPresent decision for ordinary jobs; item 11 gives it the mechanism.
