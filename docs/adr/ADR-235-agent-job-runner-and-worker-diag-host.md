# ADR-235: Agent Job Runner and the Worker-Backed DiagHost

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** `agent` (`src/runner.rs`, `src/host.rs`, `src/link.rs`, `src/policy.rs`), `j2534-0404-service` (`tests/agent_end_to_end.rs`), `docs/system-architecture.md` 3.3 / 8.2.4

## Context

The agent (design 3.3) runs IR procedures (8.2) against a VCI through a worker service, over the worker's D-PDU API gRPC interface (7.4). The two sides do not fit directly:
- `diag_ir::DiagHost` is synchronous, and `Vm::step` calls it in the middle of an instruction (ADR-233).
- The worker client (`worker_host::client::WorkerClient`) is a tonic client, so every call is async.

The IR leaves open:
- what `ServiceRequest { service }` sends and returns;
- how a negative response, a response pending (NRC 0x78) and a missing response reach the procedure;
- where the link (protocol, CAN identifiers, timing) comes from;
- what the host does with a primitive it cannot serve yet.

Two further problems:
- The worker client sets no deadline on unary calls, so a wedged worker would block a job forever.
- Job authorization, approval and the execution preconditions (design 5.5, 5.6, 6, 8.9) do not exist yet, so the first runner must not be able to change anything on an ECU.

## Decision

1. **One blocking thread per job.**
   - `run_program` checks the runtime and the program, then waits for one `tokio::task::spawn_blocking` thread.
   - That thread opens the link, runs the VM and closes the link. It runs each gRPC sequence with `Handle::block_on` on the runtime that called `run_program`, so no future that holds worker resources can be dropped halfway.
   - `run_program` refuses a current-thread runtime (`JobError::CurrentThreadRuntime`): `block_on` from a blocking thread would have no thread left to drive the I/O. Tests use a multi-thread runtime.
   - `DiagHost` stays synchronous. An async trait waits until a second host exists.

2. **What the primitives send.**
   - `ServiceRequest { service }`: `service` is the UDS service identifier (SID). The request is that byte followed by the operand bytes. A value above 0xFF is not a SID and fails with `HostError::BadService`.
   - The result is the whole final response message, with the CAN identifier removed, as one `COPT_SENDRECV` primitive returns it.
   - `ReadDtc { mask }` sends ReadDTCInformation, reportDTCByStatusMask (ISO 14229-1:2026 clause 11.3), and returns the raw response. Decoding it is the procedure's job.
   - `RoutineControl { routine, sub }` would send RoutineControl (clause 13.2) with the routine identifier big-endian. Item 8 refuses it for now.

3. **Which responses count, and how failures surface.**
   - A send-receive takes only an answer to its own request:
     - A positive response must start with the SID plus 0x40. For a single-DID ReadDataByIdentifier, the DID must follow; for ReadDTCInformation, the sub-function without its suppress bit (clauses 10.2 and 11.3).
     - A negative response must be `7F`, the request's SID and a code.
     - The host gives the worker exactly these two patterns and checks each result again itself. Its check is the stricter one, because the worker also matches a frame shorter than the pattern.
     - An unsolicited frame, or a late answer for another DID or service, neither ends the primitive nor becomes its result.
   - A negative response is `Ok` with its bytes. The procedure inspects it.
   - The worker absorbs response pending (0x78). The link sets `CP_RC78Handling`, and `CP_RCByteOffset` to 2, because the worker looks for the code only at that offset. The host never takes a `7F SID 78 ..` result as the final response.
   - Failures are errors, and the VM state then still points at the primitive (ADR-233 item 1):
     - `NoResponse`: the primitive ended without a final response, or the send-receive deadline passed. This includes the worker's receive timeout (`PDU_ERR_EVT_RX_TIMEOUT`), which it also reports when a segmented request's flow control never came.
     - `PrimitiveFailed`: the worker reported an error event for the primitive, for example a transmit error. The request may not have been sent.
     - `EventsLost`: the worker dropped events of the link. The outcome is unknown, so the send-receive fails at once.
     - A lost VCI is not told apart yet. The worker reports it without a primitive handle and then cancels the primitive, so it ends as `NoResponse`.

4. **The link, set up before the first step.**
   - `LinkConfig` carries:
     - the protocol short name and baud rate;
     - the physical request and response CAN identifiers;
     - P2, P2* and the 0x78 completion timeout.
   - Until procedures carry their protocol declarations, the caller builds it. The IR `Protocol` table has no timings yet.
   - `LinkConfig::validate` refuses:
     - any protocol other than `ISO15765`. Item 8 judges UDS service IDs, and on raw CAN, for example, the same bytes are not UDS requests.
     - a zero timing. The worker replaces a zero P2 or P2* with its own default and reads a zero completion timeout as no limit, so its timing would no longer match the host's.
     - a timing too large for the worker's microsecond ComParams.
     - a CAN identifier above 11 bits. A 29-bit identifier needs the ID format ComParams, which the link does not set.
   - `link::open` follows the usage sequence of `docs/rpc-api-guide.md`:
     - GetModuleIds. The worker must report exactly one module: with several, the link is refused rather than opened on whichever comes first.
     - ModuleConnect, GetResourceIds for the protocol, then CreateComLogicalLink.
     - SetComParam for the baud rate and timings, with ComParam IDs resolved through GetObjectId.
     - SetUniqueRespIdTable with the two identifiers, which makes the J2534 service install the flow-control filter (ADR-234).
     - ConnectComLogicalLink, then SubscribeEvent on the link.
   - Closing:
     - disconnects and destroys the link, then disconnects the module;
     - returns the first failure after trying every step.
   - When a step of `open` fails, it releases what it had acquired:
     - A ModuleConnect that fails or times out is followed by a best-effort ModuleDisconnect, since the connect may have completed on the worker after the deadline.
     - The same call releases a link whose CreateComLogicalLink timed out before its handle came back, because disconnecting a module releases all its links (ISO 22900-2:2022 clause 8.4.30).
   - The job thread closes the link however the job ends, a panic in the step loop included. A failure to close is logged and does not replace the job's result.

5. **Primitives without a backend fail.**
   - SecurityAccess, FlashTransfer, HmiRequest, RecordInput and MonitorCapture return `Err(HostError::Unsupported)`. They never return `Ok(None)`: that means "waiting" and would make the runner poll forever.
   - A `Waiting` outcome other than a timer ends the job with `JobError::Unanswerable`.
   - `Wait` is timed by the agent's monotonic clock, keyed by the inquiry ID, and polled every `JobLimits::wait_poll` (at least 1 ms).

6. **Deadlines.**
   - Each unary call of the link and the host gets 5 s (`Timings::unary`). A call that runs out fails with `WorkerUnresponsive`.
   - A send-receive gets one budget: P2, plus the 0x78 completion timeout, plus P2*, plus 2 s.
     - The budget is counted from before `StartComPrimitive`, and that call is bounded by the smaller of the unary deadline and the budget.
     - The worker's own worst case is P2 plus the completion timeout. P2* and the 2 s are margin, so the worker's timeouts fire first unless sending the request takes longer.
   - The event stream itself has no deadline. A general unary deadline inside `worker-host` is separate work.

7. **Scope of the minimal runner.**
   - Before the link opens, `run_program` checks:
     - the program's schema version, against the VM;
     - the policy of item 8.
   - A program that fails either check never reaches the bus. One that passes can still stop at an unsupported primitive (item 5), after earlier reads have gone out.
   - A VM or host error ends the job and reports the `pc` of the failed instruction. `JobLimits::max_steps` bounds the step count. Nothing is repeated.
   - Dropping the `run_program` future cancels the job:
     - The thread checks the flag before opening the link and before every instruction.
     - The call or primitive in flight finishes, and the link is closed.
     - Runtime shutdown is the exception: the thread's calls then fail, and the worker process releases the device when it exits.
   - Journaling, idempotency and section policy (ADR-229, ADR-233 items 2 and 3) belong to a later runner that wraps the same step loop.

8. **Read-only requests only.**
   - `ServiceRequest` is allowed only for ReadDTCInformation (0x19) and ReadDataByIdentifier (0x22). `ReadDtc` always sends 0x19.
   - Everything else is refused:
     - session changes, resets, writes, DTC clearing, communication control and transfers;
     - every `RoutineControl`: a routine can erase or actuate, and its number does not say which;
     - every `SecurityAccess` and `FlashTransfer`;
     - TesterPresent (0x3E). It changes no data, but it keeps the ECU's current session alive, and only a job that owns the session may do that (the S3 keep-alive of design 5.6 and 8.2.5).
   - The rule is checked at two points:
     - Before the link opens, `run_program` scans the whole program and refuses the first offending instruction (`JobError::Refused` with its `pc`). The scan matches every instruction explicitly, so a new instruction must be classified before the crate compiles.
     - The host checks the first byte of every request in `send_recv`, which every primitive sends through (`HostError::NotAllowed`). A different runner, a VM state restored from a journal, or a primitive that later gets a backend is held to the same rule.
   - The policy layer of design 5.5, 5.6, 6 and 8.9 will replace the allowlist, not the two checkpoints.

## Consequences

- Procedures that change anything on the ECU cannot run yet. Their first offending instruction is refused before the link opens. The same applies to any procedure that needs seed-key, HMI, records, flash or monitor capture, which stops at the first such primitive.
- A negative response is a value, not an error, so a procedure that ignores one carries on. The transpiler must emit the checks.
- Limits of the response matching (item 3):
  - A malformed answer shorter than the expected pattern, for example a lone `62`, ends the primitive on the worker. The host rejects it, and the job fails with `NoResponse` instead of waiting for a complete answer. That is safe, since the wrong bytes are never taken, and it needs an ECU that sends malformed frames, so the worker's matching is left as it is.
  - A request for several DIDs is matched on the SID alone, since the standard does not fix the order of the DIDs in the response.
  - UDS responses carry no sequence number, so an ECU answering the same request twice cannot be told apart from one answer.
  - A request whose positive response is suppressed (the suppress bit set) ends in `NoResponse`. The host does not yet know which requests expect no answer.
- One job holds the worker, and one blocking-pool thread, for its whole run. Closing a link disconnects the module, which releases every link on it. Two consequences follow:
  - After dropping a job, the caller must not start another on the same worker until the dropped job's thread has closed its link.
  - Two jobs must never run on one worker at the same time, because the first to finish would release the other's link.
  - `run_program` takes a cloneable client and enforces neither. The job scheduler must hold a per-worker guard (backlog).
- The link supports only `ISO15765` with 11-bit identifiers. It sets no CAN frame padding and no addressing format. `sim-ecu` needs none of them, but real ECUs may.
- The end-to-end test runs `run_program` against the real `j2534-0404-service` binary, `sim-vci` and `sim-ecu`. It checks a positive and a negative response, both returned as `Ok`. The response-pending path through the worker is tested end to end in `tests/sim_vci_response_pending.rs`, with 0x78 chains injected into `sim-ecu` (`Fault::ResponsePending`).
