# ADR-235: Agent Job Runner and the Worker-Backed DiagHost

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** `agent` (`src/runner.rs`, `src/host.rs`, `src/link.rs`, `src/policy.rs`), `j2534-0404-service` (`tests/agent_end_to_end.rs`), `docs/system-architecture.md` 3.3 / 8.2.4

## Context

The agent (design 3.3) runs IR procedures (8.2) against a VCI through a worker service, over the worker's D-PDU API gRPC interface (7.4). The two sides do not fit directly:
- `diag_ir::DiagHost` is synchronous, and `Vm::step` calls it in the middle of an instruction (ADR-233).
- The worker client (`worker_host::client::WorkerClient`) is a tonic client, so every call is async.

The IR also leaves some things open:
- What `ServiceRequest { service }` sends and what it returns.
- How a negative response, a response pending (NRC 0x78) and a missing response reach the procedure.
- Where the link (protocol, CAN identifiers, timing) comes from.
- What the host does with a primitive it cannot serve yet.

The worker client sets no deadline on unary calls, so a wedged worker would block a job forever.

## Decision

1. **Bridge: one blocking thread per job.**
   - `run_program` only checks the runtime and the program, then waits for a `tokio::task::spawn_blocking` thread.
   - That thread opens the link, runs the VM and closes the link, each gRPC sequence with `Handle::block_on` on the runtime that called `run_program`. No future that holds worker resources can be dropped halfway.
   - `DiagHost` stays synchronous; an async trait waits until a second host exists.
   - `run_program` refuses a current-thread runtime (`JobError::CurrentThreadRuntime`): `block_on` from a blocking thread would have no worker thread to drive the I/O. Tests use `#[tokio::test(flavor = "multi_thread")]`.
2. **`ServiceRequest { service }`**
   - `service` is the UDS service identifier. The request on the wire is that byte followed by the operand bytes, so values above 0xFF fail with `HostError::BadService`.
   - The result is the whole final response message (A_Data with the CAN identifier removed), as one `COPT_SENDRECV` primitive returns it.
   - `ReadDtc { mask }` sends ReadDTCInformation, reportDTCByStatusMask (ISO 14229-1:2026 clause 11.3), and returns the raw response; decoding it is the procedure's job.
   - `RoutineControl { routine, sub }` would send RoutineControl (clause 13.2) with the routine identifier big-endian. It is refused for now (item 8).
3. **Responses**
   - A send-receive accepts only an answer to its own request. A positive response must begin with the SID plus 0x40 followed by what the server echoes of the request: the DID of a single-DID ReadDataByIdentifier, or the sub-function, without its suppress bit, of ReadDTCInformation and TesterPresent (ISO 14229-1:2026 clauses 9.7, 10.2, 11.3). A negative response must name the request's SID.
   - The host asks the worker for exactly these two patterns and checks each result again itself. The host's check is the stricter one, since the worker also matches a response shorter than the pattern. An unsolicited frame from the ECU, or a late answer for another DID or service, neither ends the primitive nor becomes its result.
   - A request for several DIDs is matched on the SID alone, since the standard does not fix the order of the DIDs in the response. UDS responses carry no sequence number, so a second answer to the same request (an ECU answering twice) cannot be told apart from the first.
   - A negative response is `Ok` with its bytes (`7F`, SID, NRC); the procedure inspects it.
   - Response pending (0x78) is the worker's job. The link sets `CP_RC78Handling`, and `CP_RCByteOffset` to 2, the code's position in a negative response; the worker looks for the code only at that offset. The host also never takes a `7F SID 78` result as the final response, so a chain that ends without one is not handed to the procedure as its answer.
   - No final response is an error, and the VM state then still points at the primitive (ADR-233 item 1). The send-receive deadline passing, or the primitive ending without one, gives `Err(HostError::NoResponse)`. When the worker reported an error event for the primitive (a transmit error, a lost VCI), the error is `HostError::PrimitiveFailed` with that event, since the request may not have been sent.
4. **Link set-up before the first step.** `LinkConfig` carries:
   - protocol short name, baud rate, physical request and response CAN identifiers;
   - P2, P2* and the 0x78 completion timeout. The IR `Protocol` table does not carry these timings yet.

   `LinkConfig::validate` refuses a zero timing (the worker reads it as "no limit", or as a 0x78 window that ends at once), a timing that does not fit the worker's microsecond ComParams, and a CAN identifier above 29 bits. `link::open` then runs the usage sequence of `docs/rpc-api-guide.md` before step 0:
   - GetModuleIds, ModuleConnect and GetResourceIds for the protocol, then CreateComLogicalLink;
   - SetComParam for the baud rate and timings, with ComParam IDs resolved through GetObjectId;
   - SetUniqueRespIdTable with the two identifiers, which makes the J2534 service install the flow-control filter (ADR-234);
   - ConnectComLogicalLink, then SubscribeEvent on the link.

   Closing disconnects and destroys the link and disconnects the module. `link::open` closes whatever it had opened when a later step fails. A ModuleConnect that fails or times out is followed by a best-effort ModuleDisconnect, since the connect may have completed on the worker after the deadline. The same call releases a link whose CreateComLogicalLink timed out before its handle came back, because the worker releases all of a module's links when the module is disconnected (ISO 22900-2:2022 clause 8.4.30). `run_program` closes the link however the job ends, a panic in the step loop included. A failure to close is logged and does not replace the job's result, since the results are already on the stack. Until procedures carry their protocol declarations, the caller builds the `LinkConfig`.
5. **Primitives without a backend fail.** SecurityAccess, FlashTransfer, HmiRequest, RecordInput and MonitorCapture return `Err(HostError::Unsupported)`. They do not return `Ok(None)`, which means "waiting" and would make the runner poll forever. A `Waiting` outcome other than a timer ends the job with `JobError::Unanswerable`. `Wait` is answered from the agent's monotonic clock, keyed by the inquiry ID, and the runner polls it at `JobLimits::wait_poll`.
6. **Deadlines**
   - Each unary call made by the link and the host gets 5 s (`Timings::unary`), checked with `tokio::time::timeout`, and a call that runs out fails with `HostError::WorkerUnresponsive`.
   - A send-receive gets P2 plus the 0x78 completion timeout plus P2* plus 2 s. The worker's worst case is P2 plus the completion timeout, so its own timeouts fire first unless sending the request takes more than the margin.
   - The event stream itself is unbounded. A general deadline for unary calls inside `worker-host` is separate work.
7. **The minimal runner holds no policy yet.**
   - `run_program` checks the program against the VM (schema version) before it opens the link, so a program this agent cannot run never reaches the bus.
   - A VM or host error ends the job, reporting the `pc` of the failed instruction, and `JobLimits::max_steps` bounds the step count. The job does not repeat anything.
   - Dropping the `run_program` future cancels the job. The thread checks the flag before opening the link and before every instruction. The call or primitive in flight finishes (a send-receive can take up to its ceiling), nothing the job owns is dropped mid-sequence, and the link is closed. Runtime shutdown is the exception: the thread's calls then fail, and the worker process releases the device when it exits.
   - Journaling, idempotency and section policy (ADR-229, ADR-233 items 2 and 3) belong to a later runner, which wraps the same step loop.
8. **Read-only requests only.** Until job authorization, approval and the execution preconditions exist (design 5.5, 5.6, 6, 8.9), the runner sends only requests that change nothing on the ECU:
   - `ServiceRequest` is allowed only for ReadDTCInformation (0x19), ReadDataByIdentifier (0x22) and TesterPresent (0x3E). `ReadDtc` always sends 0x19.
   - Session changes, resets, writes, DTC clearing, communication control and transfers are refused, as are every `RoutineControl`, `SecurityAccess` and `FlashTransfer` instruction. A routine can erase or actuate, and its number alone does not say which. A value above 0xFF is not a service ID and fails as `HostError::BadService` before the allowlist is consulted.
   - The check runs twice. `run_program` scans the program before it opens the link and refuses the first offending instruction with `JobError::Refused` and its `pc`, so nothing reaches the bus. The host repeats the check on the first byte of every request it sends, at the one function every primitive sends through (`HostError::NotAllowed`). A different runner, a VM state restored from a journal, or a primitive that later gets a backend is held to the same rule. The program scan matches every instruction explicitly, so a new instruction must be classified before it compiles.
   - The later policy layer replaces the allowlist, not the two checkpoints.

## Consequences

- A procedure that changes the session, writes, clears DTCs, runs routines or resets the ECU is refused before the link opens, with its `pc`. The end-to-end test reads only.
- A cancelled job's thread keeps the worker after its future is gone: it finishes the call or primitive in flight (up to the send-receive ceiling) and then closes the link, and closing disconnects the module, which releases every link on it. A caller that drops a job must not start another on the same worker until that thread has ended. `run_program` cannot express that yet; the job scheduler has to (backlog).

- One job holds one blocking-pool thread for its whole run. That is fine for one job per VCI, but the agent's job scheduler must bound concurrent jobs.
- The end-to-end test runs `run_program` against the real `j2534-0404-service` binary, `sim-vci` and `sim-ecu`. It checks a positive response and a negative response, both arriving as `Ok`. `sim-ecu` never sends 0x78, so the response-pending path through the worker is not covered end to end.
- A negative response is a value, not an error. A procedure that ignores it carries on, so the transpiler must emit the checks.
- Requests whose positive response is suppressed (the sub-function's suppress bit, for example tester present `3E 80`) end in `NoResponse`. A procedure cannot send them until the host knows which requests expect no answer.
- No CAN frame padding or addressing format ComParam is set. `sim-ecu` does not need them, but real ECUs may.
- A program that needs seed-key, HMI, records, flash or monitor capture cannot run until those backends exist. It fails at its first such primitive, with the state pointing at it.
