# ADR-325: Procedures That Address Several ECUs

**Date:** 2026-10-17
**Status:** Accepted
**Affects:** `diag-ir` (`Program`, `Op`, `VmState`, `DiagHost`), `diag-frontend` (procedure
targets, `diag.ecu`, lowering per combination of variants), `agent` (`Job`, `check_job`, one
link per target, keep-alive, restart order), the workers' tester-present ComParams, `sim-ecu`
(a gateway role), design 8.2.4, 8.2.6, 8.10; ADR-229, ADR-264 item 4, ADR-307, ADR-311, ADR-318
item 1

## Context

A procedure is lowered against one variant's declaration part and shipped in that variant's
document (ADR-307, ADR-311); a job holds that one program and one document (ADR-318), and the
agent opens one link with one request and response identifier pair before the first
instruction. No instruction names an ECU, so a procedure can address exactly one ECU.

Many jobs need more than one in a single procedure:

- Functions the vehicle protects behind a security gateway: the gateway ECU is put into an
  extended session and unlocked with security access, and must stay in that session while the
  tester works on the ECU behind it (a reprogramming, an input-output control, a routine).
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
here, a procedure names several targets and the IR switches between them; a gateway unlock is
then an ordinary shared function written on top.

## Decision

1. **A procedure names its targets.** A procedure declares the ECUs it addresses as targets,
   each a role name the procedure chooses (`gateway`, `engine`) bound to an ECU definition. The
   ECU of the definition the procedure belongs to is the primary target, which a procedure that
   declares no other target addresses alone, as before. A target is reached physically, by the
   addressing of its definition's protocol. Targets addressed functionally (one request that
   several ECUs answer) are not part of this decision; they are decided with the functional
   requests of the generic OBD package, and the target model leaves room for them.
2. **Starlark addresses a target through a handle.** `diag.ecu(role)` returns the handle of a
   declared target, with a constant role, and the diagnostic calls are available on the handle
   (`gw.security_access(level)`, `eng.request(...)`). The module-level calls keep addressing the
   primary target, so existing procedures do not change. Each call is lowered against its
   target's declaration part.
3. **The IR selects the target.** `Program` lists its targets, the primary first. A
   `SelectTarget` instruction, appended after the existing ones, makes one of them current; the
   diagnostic primitives and `DecodeField` act on the current target, its link and its
   declarations, and the VM tells the host which target is current. The current target is part
   of `VmState`, so the journal records it with the rest of the state and a restored state
   resumes on the right target. A program starts on the primary target. The lowering emits
   `SelectTarget` where the target changes, so the existing instruction encodings stay as they
   are.
4. **Lowered once per combination of variants.** A procedure is lowered for each combination of
   one variant per target, extending ADR-311's per-variant lowering, and must lower for every
   combination. The program for a combination is carried, as now, in the primary variant's
   document; it names, for each other target, the content hash (design 8.2.6) of the
   declaration-only document of the variant it was lowered against. The conversion refuses a
   procedure whose combinations exceed a bound, so an author cannot make the package grow
   without limit; the bound is set with the implementation.
5. **A job carries one document per target.** A job holds the program, the primary document it
   came from, and the declaration-only document of each other target (amending ADR-318 item 1);
   `check_job` refuses a job whose other documents' hashes differ from the ones the program
   names, and checks each target's decodes against that target's declarations, before
   anything is sent. Each target's variant is identified as the primary's is (8.9.1, ADR-315)
   before the job is chosen.
6. **One link per target, under the same guards.** Before the first instruction the agent opens
   a link to each target, all on the job's VCI, under the job's per-VCI lock, per-vehicle lock
   and, for a writing job, reprogramming slot (8.8). Targets on buses the VCI cannot open at the
   same time are refused before anything is sent.
7. **Sessions are kept alive by the worker, per target.** A target can be kept alive: the agent
   sets the tester-present ComParams of ISO 22900-2 (`CP_TesterPresentHandling` and its
   siblings, which the J2534 worker maps as well) on that target's link, so the worker sends
   TesterPresent periodically and expects its response (`CP_TesterPresentReqRsp`), so that a
   lost answer is seen (item 8), while the VM runs and while it waits on the server, on an HMI
   response or on a `Wait`. A procedure turns keep-alive on and off per
   target (after an ECUReset of that target, or to let a session lapse). The single-target job
   uses the same mechanism for its own session.
8. **A lost keep-alive follows the section attributes.** When the worker reports that a target
   stopped answering its keep-alive, or answered it negatively, the runner applies the rule
   that design 8.10.1 sets for an expired authentication: in an interruptible section the job
   ends as failed at the next instruction boundary; in an uninterruptible section it continues
   to the end of the section and then ends; in a section marked recovery required it ends in
   on-site intervention. The agent does not unlock a target again by itself.
9. **At most one target is reprogrammed.** A job's flash sessions and recovery plans (ADR-245)
   all belong to one target, the reprogramming target, whose identity the journal records and
   the restart checks (ADR-229). Every other target may receive any request the job's policy
   allows (security access, writes, input-output control, routines), but no download. The
   conversion refuses a procedure that has flash sessions on two targets.
10. **A restart re-establishes access before it reads the vehicle.** A procedure whose access to
    the reprogramming target, or to the VIN, goes through another target declares the steps
    that establish it (the gateway's session and security access) as the access steps of its
    recovery plan. A restart runs them after the locks and the server reservation and before
    the VIN read of ADR-229, and keeps those targets alive from then on; they need the server
    for the key (design 8.10), so a restart without it waits until the start deadline as any
    other online step does. The passive teardown (ADR-264 item 4) stops the keep-alive of the
    reprogramming target before it waits out that target's session, and keeps the access
    targets alive.

## Consequences

- `IR_SCHEMA_VERSION` changes with the target list and `SelectTarget`, and the journal's state
  with the current target; nothing stored outside the repository needs a migration (design
  8.2.6).
- The number of documents grows with the product of the targets' variant counts. The bound of
  item 4 keeps it finite; a definition that needs more must split its procedure.
- A gateway unlock is written once as a function and shared between procedures through the
  shared-file `load` of the Starlark subset, which does not exist yet; until it does, each
  procedure carries its own copy.
- The worker must report a failed keep-alive to the agent. Whether the J2534 worker's
  tester-present dispatch reports one today was not checked.
- Functional targets, needed for the CommunicationControl and ControlDTCSetting broadcast of a
  reprogramming, remain open (item 1). Until they exist, a procedure can send those requests to
  each ECU through a physical target.
- Other targets' state after a crash (communication disabled, DTC recording off) returns to
  normal when their sessions time out; the restart's wait covers only the reprogramming
  target's session, so a procedure that changes another target's state should keep that target
  among its access steps or restore it explicitly.
