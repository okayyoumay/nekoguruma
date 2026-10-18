# ADR-325: Procedures That Address Several ECUs

**Date:** 2026-10-17
**Status:** Accepted
**Affects:** `diag-ir` (`Program`, `Op`, `VmState`, `DiagHost`, the restart declaration),
`diag-frontend` (procedure targets, `diag.ecu`, access programs, lowering per combination of
variants), `agent` (`Job`, `check_job`, policy, one link per target, keep-alive, run and restart
order), the workers' cyclic tester-present (kept off), `sim-ecu` (a gateway role), design 8.2.4, 8.2.5,
8.2.6, 5.6, 8.10, 8.10.1; ADR-229 item 2, ADR-233 item 3, ADR-235 item 8, ADR-244, ADR-245, ADR-247 item 1, ADR-257,
ADR-264 item 4, ADR-280, ADR-293, ADR-307 items 1 and 5, ADR-309 item 3, ADR-311 item 1, ADR-313 items 1 and 2, ADR-316, ADR-318 items 1, 3 and 4,
ADR-321, ADR-322 items 1 and 3, ADR-324

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
   definition the procedure belongs to is the primary target, which a procedure that declares no
   other target addresses alone, as before. A target is reached physically, by the addressing of
   its definition's protocol. Targets addressed functionally (one request that several ECUs
   answer) are not part of this decision; they are decided with the functional requests of the
   generic OBD package, and the target model leaves room for them. The conversion refuses more
   than eight targets per procedure, and two roles bound to the same definition (the primary's
   included), since `SelectTarget` would then switch between names of one ECU and a step meant
   for one target would reach the other.
2. **Starlark addresses a target through a handle.** `diag.ecu(role)` returns the handle of a
   declared target; the role must be a constant, so every handle is known at conversion. The
   diagnostic calls are available on the handle (`gw.security_access(level)`,
   `eng.request(...)`). Handles follow ADR-300's rules for function values: bound to a name or
   passed as an argument, never chosen by a conditional expression or stored in a container, and
   a function that receives a handle is specialized per handle. The module-level calls keep addressing the
   primary target, so existing procedures do not change. Each call is lowered against its
   target's declaration part.
3. **The IR selects the target.** `Program` lists its targets in declaration order, the
   primary first, each marked as an access target or not (item 5). A
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
   to. The VIN source may be on any target, except in a procedure with access targets (below);
   the hardware identity, the software version and every flash plan are on the reprogramming
   target (item 9). `Program::validate`, which every load runs, refuses flash plans on two
   targets, a hardware identity or software version source off the reprogramming target (the
   restart compares that version before the erase and reads it back after the transfer,
   ADR-229), a download service addressed to any other target, and any instruction between a
   plan's erase and its post-transfer end that addresses another target. It also refuses a flash
   plan on an access target (item 13) and, in a procedure with several targets or an access
   target, a missing VIN source, since its first run reads the VIN to take the per-vehicle lock
   before identifying the targets that pick the combination (item 5), and, in a procedure with
   access targets, a VIN source anywhere but on the first access target; `check_job` refuses
   such a job without a target VIN, and one whose combinations declare different VIN sources,
   since the VIN is read before a combination is chosen.
5. **Access programs.** A target whose session and unlock other targets depend on (a gateway)
   can be declared an access target, with an access program: steps that only change that
   target's session and security state and read from it (session control to the default or an
   extended session only, security access, reads). The declaration also gives the target's
   session timeout, tester-present period and tester-present response window (item 11); the
   conversion and `check_job` refuse a period or a response window that is not positive, a
   period shorter than 200 ms (the tolerance ISO 14229-2 (2021) 9.5 allows a server's session
   timer, which the margin of item 12 must cover) or longer than a fifth of the session timeout,
   and a response window shorter than the link's declared P2 (so the module's own receive
   timeout ends an unanswered probe before the agent gives it up) or longer than a fifth of the
   session timeout, so that even with one probe unanswered the next answer is due a fifth of the
   timeout before the target would be counted lost (item 12). Several access targets (a gateway
   behind a gateway) run their access programs in declaration order. The access program is
   lowered on its own, once per variant of the access target, and travels with the job; the
   conversion and `check_job` refuse any other service in it. Every target's variant is
   identified outside the combination programs, and the first access target's before the
   per-vehicle lock and the safety preconditions of 8.9, so in a procedure with several targets
   or with an access target, a one-target procedure whose primary is an access target included,
   the conversion and `check_job` also refuse a variant pattern (8.9.1) of any target whose
   service is not a read of the kind an access program may send; identification then sends only
   reads, whatever the job's class. No access program runs before the job holds the per-vehicle
   lock, since a session change or an unlock on a shared gateway could end the session that
   another job of this device on the same vehicle depends on (8.8). So the VIN is read, without
   any access program, from the first access target, which a locked gateway answers with reads.
   A first run goes in this order: identify the first access target and read the VIN from it;
   when the VIN matches, take the per-vehicle lock (8.8), waiting for it with nothing more sent
   while another job holds it; then identify the first access target again, since the job that
   held the lock may have reflashed it, and read the VIN again with the variant now identified,
   since the vehicle may have changed during the wait; then, for each access target in
   declaration order, run its access program at once, identifying each one after the first just
   before its program, so that an inner gateway is reached through an unlocked outer one; then
   identify the other targets and pick the combination (item 7). A VIN that differs, at either
   read, ends a first run in the 8.9.1 pre-check with only reads sent, and a lock failure leaves
   only those reads sent. A procedure with several targets and no access target goes the same
   way with its VIN-source target in place of the first access target: identify that target and
   read the VIN, take the per-vehicle lock, identify it again and read the VIN again, then
   identify the other targets and pick the combination, so no variant that picks the combination
   is identified before the lock is held. A restart (8.2.5) identifies the first access target
   and reads the VIN from it in ADR-229's order (item 2 step 2), since that target may have been
   reflashed after the journal recorded its variant, and, once it holds the per-vehicle lock,
   identifies it again after any wait, reads the VIN again and requires it to match, as a first
   run does, and only then runs the access programs in the same way before any other step; a
   restart of a procedure with several targets and no access target does the same with its
   VIN-source target, identifying it again after any wait for the lock and reading the VIN
   again, so a vehicle exchanged during the wait is caught before any recovery step. When that
   VIN-source target is the reprogramming target and the journal has reached its flash plan, it
   is not identified at either point, since it may answer only from its bootloader (item 7's
   exception): both VIN reads go through the recorded combination's declarations for it, as
   ADR-229 reads a one-target restart's VIN, and ADR-229's checks validate it. Since its VIN now
   comes from an access target rather than from the ECU being reprogrammed, a step 2 VIN read
   that gets no answer or a negative response ends the restart in on-site intervention
   (`IdentityNotEstablished`, which a later resume may retry, ADR-290) instead of the passive
   teardown ADR-229 takes there, which would reach the reprogramming target's default-session
   confirmation without an unlocked gateway. A step of an access program that fails ends the job
   as a lost access target would at that point (item 12). A failure in this order is held for
   the access targets that already have a loss clock, which starts at the first final response
   of a target's access program (item 12), and is handled by its own rule only once none of them
   is lost; a target without a clock yet holds nothing. A loss counted while an access program
   runs ends the job as a failing step of it does. Every run also runs them again after any
   reopening of the links (a worker crash or VCI disconnect that the job survives) before the
   program continues; when such a rerun fails, or a loss is counted during it, while the VM is
   inside an uninterruptible section, the job ends in on-site intervention, since the section
   cannot continue to its end without the gateway. The main program may still select an access
   target, but only for reads: the conversion and `check_job` refuse any other service a
   combination program addresses to an access target, since a session change, a reset or a new
   security access there would undo the access program while TesterPresent kept being answered
   in the default session, so the loss clock would not see it. A procedure that must change a
   gateway's own state does so in a separate procedure that does not declare it an access
   target.
6. **Lowered once per combination of variants.** The main program is lowered for each
   combination of one variant per target, extending ADR-311's per-variant lowering, and must
   lower for every combination; the scoping of a procedure to some variants that ADR-311 item 2
   leaves open applies to targets as well. Each combination's program goes in its own document
   for the primary variant (amending ADR-307 item 1's one document per variant and procedure),
   which names the procedure it was lowered from by its definition, its file (ADR-307 item 3)
   and the content hash of that file's source, so two revisions of a package that keep the name
   but change the source name different procedures, and, for every target, the primary included,
   the content hash (design 8.2.6) of that variant's declaration-only document, and, for every
   target, the hashes of the declaration-only documents of all of its candidate variants, the
   same list in every combination of the procedure; the conversion emits a declaration-only
   document for every variant of every target of a procedure, the primary's included. The
   document's digest covers every definition its program was lowered against (amending ADR-307
   item 5). The conversion refuses a procedure whose combinations exceed a bound set with the
   implementation, so the package cannot grow without limit.
7. **A job holds the documents of every combination it may meet.** Since the variants behind an
   access target are known only after its access program has run, a job names, by content hash,
   its access programs, the declaration-only documents of every candidate variant of every
   target, and every combination's document (amending ADR-318 item 1), and the agent holds all
   of them before anything is sent; the bound of item 6 keeps their number finite. Each access
   program names its procedure, as a combination document does, its access target and the hash
   of the declaration-only document of the variant it was lowered for, and the agent runs the
   one whose variant it has just identified; `check_job` refuses an access program whose named
   target is not an access target or whose hash names no document the job holds, and a job that
   lacks one, or holds more than one, for any candidate variant of an access target. `check_job`
   validates all of them then: it refuses a job whose combination documents and access programs
   do not all name one procedure, source hash included, since otherwise the variants identified
   could pick a program of another procedure, or of another revision of the same one, that has
   the same targets and candidates (one source lowered against the same named declaration-only
   documents gives the same programs, so documents of two revisions that agree on both are
   interchangeable), and it also refuses a combination document whose named hashes do not match
   the documents the job holds, combination documents that name different candidate lists, a job
   whose declaration-only documents are not exactly the documents of that list, and a set of
   combination documents that lacks one, or holds more than one, for any tuple of one candidate
   variant per target of that list; the list is signed with the combination documents, so a job
   cannot leave out a variant together with every combination that contains it, so the variants
   identified always pick exactly one program; it checks each combination's decodes against its
   targets' declarations, refuses flash plans on more than one target, and refuses an access
   program with any service item 5 does not allow; it checks each access program's decodes and
   services against the declarations of the declaration-only document that program names, as it
   checks a combination's, so an access program that would fail while decoding is refused before
   it changes the gateway's session or security state. The agent identifies each target's
   variant as the primary's is identified (8.9.1, ADR-315), through those declaration-only
   documents, and the combination its variants pick is the program that runs. Whether the job
   writes, which policy class applies and whether it takes the reprogramming slot are fixed
   before anything is sent from every combination the job holds: the job is a writing job when
   any of them writes, takes the reprogramming slot when any of them writes (ADR-257; an access
   program's services alone do not count, item 10), and is held to the policy class that covers
   all of them, so the chosen combination never needs a guard the job lacks. A job keeps a
   journal when any of its combinations has a flash plan or a section marked recovery required
   (ADR-293), whether or not it writes; such a job creates its journal before anything is sent,
   as now (ADR-252, ADR-293), and commits a combination record holding the chosen document's
   hash after identification and before the chosen program's first instruction. A restart of a
   journal with that record uses that document (amending ADR-318 item 4's journal that records
   no document; the caller still supplies the documents) and, once the VIN matches and the
   per-vehicle lock is held, identifies every target again; a variant that differs from the
   recorded combination ends the restart in on-site intervention. The one exception is the
   reprogramming target once the journal has reached its flash plan, whether inside the plan or
   past its end (ADR-271's recovery of a completed transfer): that ECU may answer only from its
   bootloader, or as the variant the new software makes it, so it is not identified again here,
   and ADR-229's hardware identity and state checks and ADR-271's checks validate it instead,
   with the recorded document kept. The record is part of the checkpoint summary a handover
   carries (design 8.2.5), and the receiving device's journal starts from it, so a handed-over
   restart uses the same document. A journal without the record holds no step of the program,
   since only access programs and identification reads ran, so its job starts again as a first
   run does. The host and the restart carry these documents in place of the single one (amending
   ADR-318 item 3).
8. **One link per target, under the same guards.** Before anything is sent the agent opens a
   link to each target, all on the job's VCI, under the job's per-VCI lock and, for a writing
   job, the reprogramming slot (8.8, ADR-257); the per-vehicle lock is still taken when the VIN
   first matches. Targets on buses the VCI cannot open at the same time are refused before
   anything is sent, and so are two targets whose links share a request or response identifier
   on the same bus, since their traffic would reach, or be answered by, the same ECU whichever
   target is current; the conversion refuses the same over the candidate variants' declared
   links. Since the link is opened before the variant is identified, the conversion and
   `check_job` refuse a target whose candidate variants differ in the link settings (protocol,
   identifiers, addressing, bus parameters); probing a variant over its own link is not part of
   this decision. Each target's link is the one its variants declare (ADR-324); on an access
   target a declared tester-present ComParam is refused, since the agent sends TesterPresent
   itself there and a second sender would make answers unattributable (item 11).
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
11. **The agent keeps access targets alive.** For each access target, from the start of its
    access program to the end of the job (so the session survives the server round trip of its
    own security access), the agent itself sends a TesterPresent with the positive response not
    suppressed on that target's link, as an ordinary send-receive primitive with the positive
    and the negative response expected; once a session-control request of the access program to
    a session other than the default one has had its positive response, the probe is instead a
    ReadDataByIdentifier of the active diagnostic session (identifier F186, ISO 14229-1 (2026)
    Annex C.1), which restarts the session timer as any request does and shows whether the
    session is still the one that response confirmed. The agent tracks that expected session at
    run time from the access program's own session-control responses (a later positive response
    to the default session puts the probe back to TesterPresent), so a probe queued before the
    session change completes still goes as TesterPresent and a read the access program makes
    first cannot make the gateway look lost. The conversion and `check_job` refuse an access
    program with a session control to a non-default session when any candidate variant of its
    target does not declare that identifier. The agent sends the probe from a task of its own on
    the runtime, so it runs while the VM runs and while it waits on the server, an HMI response
    or a `Wait`. A probe goes every period the declaration gives, measured from the previous
    probe's send, as soon as the link is free; the agent waits up to the declared response
    window for the probe's final response and then cancels the primitive. The link is shared
    with the VM's own requests to that target one exchange at a time, in arrival order, so a
    request of the VM to that target waits at most one probe exchange and a probe waits at most
    one exchange of the VM (ISO 14229-2 (2021) 10.1.4.2 sends a physically addressed
    TesterPresent only while no other request is outstanding). The probe is the agent's message,
    not the procedure's, so the policy's exclusion of TesterPresent from read-only jobs does not
    apply to it; it is sent only on an access target's link and only once that target's access
    program has started, the session being the job's own, and the procedure cannot send anything
    through it. The workers' cyclic tester-present of ISO 22900-2 (2022) Annex B stays off on an
    access target's link (the agent sets none of its ComParams and refuses a declared one, item
    8): a second sender on the same link would make answers unattributable, since the J2534
    worker discards any frame that matches its own expected tester-present response within its
    window, and that worker reports no tester-present error event anyway. A probe must also be
    able to go while a request to another target on the same channel waits for its final
    response, since a reprogramming's erase and check routines behind a gateway chain
    response-pending for longer than the gateway's session timeout, and routed traffic does not
    restart the gateway's timer (ISO 14229-2 (2021) 9.5 restarts it only on a request the
    gateway's own server receives). That is the worker's duty: it sends a queued send-receive on
    one logical link while another logical link of the same channel is in its receive phase. ISO
    22900-2 (2022) models logical links that share a physical resource as independent (an
    application creates several on one resource, 8.4.9, and takes `PDULockResource`, 8.4.13,
    only when it wants exclusivity), so the native worker, which starts the primitive in the
    module and returns, leaves this to the module, and the real-hardware checks (design 13.5)
    verify it per VCI with a response-pending chain on a link behind a gateway on the same
    channel. The J2534 worker does not meet it yet: its one poll task per channel stays in the
    receive phase of one link's send-receive and serves only its own cyclic tester-present
    meanwhile, so a sibling link's probe waits for the whole chain and the gateway really
    lapses, which the agent counts as the loss it is. Until that worker dispatches sibling sends
    during a receive phase, a job with access targets on it loses the gateway on any exchange
    behind it longer than the session timeout less one period. The same mechanism can keep a
    single target's own session alive, which is decided separately.
12. **A lost access target ends the job by the section rules.** A session probe (item 11) whose
    answer reports another session than the one the access program's last positive
    session-control response confirmed, or is negative, counts the target lost at once, since a
    server starts in its default session (ISO 14229-1 (2026) 9.2.1) and a reset or a session
    timeout there drops the unlock with it, while TesterPresent and reads would still be
    answered. Otherwise the agent counts as an access target's good answer only a final
    response, positive or negative (a response pending is none; the worker waits it out), to a
    request the agent itself sent on that target's link, a probe of item 11 or one of the VM's;
    the first final response of the target's access program is the first good answer, so the
    target has a loss clock from then on, its access program's own waits included, and a loss
    counted before that program ends ends the job as a failing step of it does (item 5); without
    that clock a gateway that relocked during a wait inside its access program would go unseen,
    since a TesterPresent is answered in the default session too. The agent keeps, for that
    link, the send times of the requests that may still be owed an answer, each read from its
    own monotonic clock before the send call: every request it sends, probes and the VM's alike,
    joins them, and every final response it sees removes the oldest one and is dated by it, so a
    request that ended without a final response the agent saw (cancelled, timed out, failed to
    send or lost from the event stream) stays among them. The ECU answers each request at most
    once, and a worker attributes a frame to whichever active primitive expects it, with nothing
    on the wire to tell two TesterPresents apart, so a late answer to an unanswered request can
    be credited to a later one, and several such answers can arrive one after another; matching
    each answer to the oldest request still owed one keeps every credited date at or before the
    send of the request the answer may really belong to. An answer that never arrives leaves its
    request among them for the rest of the job on that link (until the links are reopened and
    the access program runs again), so each such request delays every later credited date by
    about one period; one is within the bounds of item 5, and several in a job count the target
    lost, which errs toward a loss and is accepted. Nothing else is credited: not silence, not a
    probe that was cancelled or ended without a final response, not an answer the agent cannot
    attribute, not any time the worker or the module reports, and a lost-events notification on
    that link leaves the probe in flight uncredited. The ECU's session timer stops when it
    receives a request from the client that opened the session and restarts when its final
    response is complete (ISO 14229-2 (2021) 9.5), so the session is held at least until the
    credited date plus the declared timeout, and every delay in the worker, in event delivery or
    in the agent only makes a credited date older, never newer. It counts the target as lost
    once that last good answer is older than the declared session timeout less one period, the
    margin that covers the timer's tolerance, and latches the loss: a later good answer does not
    restore the target, since the ECU may have relocked meanwhile and a TesterPresent is
    answered in the default session as well. Because the link carries one exchange at a time, an
    exchange of the VM with the access target itself that outlasts the timeout less one period
    after the last good answer counts the target lost even though the ECU's timer was stopped
    during it; this errs toward a loss and is accepted, since nothing the agent observes proves
    more. An exchange with another target on the same channel is not such a case: the probe must
    go meanwhile (item 11), and a worker that holds it back loses the gateway in fact. On a loss
    the agent adapts design 8.10.1's rule for an expired authentication, without the renewal:
    outside any section and in an interruptible section the job ends at the next instruction
    boundary as failed, terminal as ADR-280's declared failure and journaled the same way, with
    its own failure code (a new edge from Writing to Failed in design 5.6); in an
    uninterruptible section it continues to the section's end and then ends the same way; in a
    section marked recovery required it ends in on-site intervention. In a restart's steps
    before the erase it ends as failed when the journal holds no transfer-start marker, and in
    on-site intervention, which a later resume may retry (ADR-290), when it does, since an
    earlier erase may have left the ECU without a valid application. The agent does not run the
    access program again by itself. A request to any target that gets no answer, or whose link
    reported lost events, is held rather than handled at once, since the gateway it passed may
    be failing while every answer so far was still good: the agent holds it until every access
    target that has a loss clock has a good answer dated after the failure (while the VM holds
    the request those links are free, so a healthy gateway's probe releases the hold within
    about one period and one response window), and then handles the failure as any failed
    request is, or until one of them is counted lost, which bounds the wait by the session
    timeout and puts the outcome above in its place, except that inside an uninterruptible
    section the job ends in on-site intervention, since the held request has no answer that
    would let the section continue to its end; so a request that times out first cannot turn a
    lost gateway into an ordinary interrupted step (ADR-235). Access targets without a loss
    clock have no last good answer, so nothing waits on them, and with none that has one a
    failure is handled at once. In item 5's order this means a failure is held for the outer
    access targets whose clocks already run, so an inner target's identification that gets no
    answer because an outer gateway relocked ends as that gateway's loss, and only when none of
    them is lost is it handled by its own rule (an identification as the primary's
    identification failing does, item 7; a VIN read as item 5 says; an access-program step as a
    lost access target would). Keep-alive answers from targets that are not access targets are
    not watched: a request to such a target that fails is handled as any failed request is.
13. **An access target carries no flash plan.** The conversion and `Program::validate` refuse
    a flash plan on a target declared as an access target. Reprogramming a gateway that other
    targets are reached through, as an access target, would need the chain behind it to be suspended during the
    plan, its own post-flash variant and access program to be chosen afterwards, and the steps a
    restart reads before the plan's replay to stay off the targets behind it; that is decided
    separately. A gateway that is not declared as an access target may still be reprogrammed
    like any other target. The restart's teardown (ADR-264) resets or waits out
    the reprogramming target while the access targets stay alive.

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
- A procedure with access targets needs a vehicle whose first access target answers the VIN
  read while locked (item 5); a vehicle that answers it only behind an unlocked gateway needs
  another route, decided separately.
- A target declared as an access target cannot be reprogrammed (item 13).
- A service on an access target itself whose answer takes longer than its session timeout less
  one period, response-pending chain included, counts that target lost although the ECU's timer
  was stopped (item 12), so procedures keep their requests to an access target short.
- A worker must send a queued send-receive on one logical link while another link of the same
  channel waits for its response (item 11). The J2534 worker does not yet (one poll task per
  channel), so on it a job with access targets loses the gateway on any exchange behind it
  longer than the session timeout less one period, until that worker dispatches sibling sends
  during a receive phase; the native worker leaves it to the module, which a real-hardware check
  verifies per VCI.
- A gateway that resets or relocks on its own and answers again before the loss threshold is
  seen only by the session probe of item 11, so an access program that changes the session needs
  its target's variants to declare the active-session identifier (item 11). An access program
  that unlocks in the default session has nothing the probe can read: a relock there shows only
  as requests behind the gateway failing, each handled by its own rule rather than as a loss.
  That residual is accepted, since a gateway that guards writes and active tests in practice
  requires an extended session first.
- The agent gives up an unanswered probe through `CancelComPrimitive`, which it does not call
  yet; the workers and `sim-vci` must end a running send-receive primitive on it, or the
  response window would have to cover the whole send-receive ceiling. A late answer to a
  cancelled probe is covered by item 12's dating rule, not by the cancel.
- Whether to keep a single target's own session alive the same way, and with which loss rule,
  is the open TesterPresent decision for ordinary jobs; item 11 gives it the mechanism.
