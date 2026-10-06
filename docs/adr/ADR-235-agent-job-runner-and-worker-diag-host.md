# ADR-235: Agent Job Runner and the Worker-Backed DiagHost

**Date:** 2026-10-06
**Status:** Accepted
**Affects:** `agent` (`src/runner.rs`, `src/host.rs`, `src/link.rs`), `j2534-0404-service` (`tests/agent_end_to_end.rs`), `docs/system-architecture.md` 3.3 / 8.2.4

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
   - `run_program` opens the link on the async side.
   - It moves the VM and a `WorkerHost` to `tokio::task::spawn_blocking`.
   - The host runs each primitive with `Handle::block_on` on the runtime that called it.
   - `DiagHost` stays synchronous; an async trait waits until a second host exists.
   - `run_program` refuses a current-thread runtime (`JobError::CurrentThreadRuntime`): `block_on` from a blocking thread would have no worker thread to drive the I/O. Tests use `#[tokio::test(flavor = "multi_thread")]`.
2. **`ServiceRequest { service }`**
   - `service` is the UDS service identifier. The request on the wire is that byte followed by the operand bytes, so values above 0xFF fail with `HostError::BadService`.
   - The result is the whole final response message (A_Data with the CAN identifier removed), as one `COPT_SENDRECV` primitive returns it.
   - `ReadDtc { mask }` sends ReadDTCInformation, reportDTCByStatusMask (ISO 14229-1 clause 11.3), and returns the raw response; decoding it is the procedure's job.
   - `RoutineControl { routine, sub }` sends RoutineControl with the routine identifier big-endian.
3. **Responses**
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

   Closing disconnects and destroys the link and disconnects the module. `link::open` closes whatever it had opened when a later step fails. `run_program` closes the link however the job ends, a panic in the step loop included. A failure to close is logged and does not replace the job's result, since the results are already on the stack. Until procedures carry their protocol declarations, the caller builds the `LinkConfig`.
5. **Primitives without a backend fail.** SecurityAccess, FlashTransfer, HmiRequest, RecordInput and MonitorCapture return `Err(HostError::Unsupported)`. They do not return `Ok(None)`, which means "waiting" and would make the runner poll forever. A `Waiting` outcome other than a timer ends the job with `JobError::Unanswerable`. `Wait` is answered from the agent's monotonic clock, keyed by the inquiry ID, and the runner polls it at `JobLimits::wait_poll`.
6. **Deadlines**
   - Each unary call made by the link and the host gets 5 s (`Timings::unary`), checked with `tokio::time::timeout`, and a call that runs out fails with `HostError::WorkerUnresponsive`.
   - A send-receive gets P2 plus the 0x78 completion timeout plus P2* plus 2 s. The worker's worst case is P2 plus the completion timeout, so its own timeouts fire first unless sending the request takes more than the margin.
   - The event stream itself is unbounded. A general deadline for unary calls inside `worker-host` is separate work.
7. **The minimal runner holds no policy yet.**
   - `run_program` checks the program against the VM (schema version) before it opens the link, so a program this agent cannot run never reaches the bus.
   - A VM or host error ends the job, reporting the `pc` of the failed instruction, and `JobLimits::max_steps` bounds the step count. The job does not repeat anything.
   - Dropping the `run_program` future cancels the job. The primitive in flight finishes, the step loop stops before the next instruction, and the link is closed from the blocking thread. Journaling, idempotency and section policy (ADR-229, ADR-233 items 2 and 3) belong to a later runner, which wraps the same step loop.

## Consequences

- One job holds one blocking-pool thread for its whole run. That is fine for one job per VCI, but the agent's job scheduler must bound concurrent jobs.
- The end-to-end test runs `run_program` against the real `j2534-0404-service` binary, `sim-vci` and `sim-ecu`. It checks a positive response and a negative response, both arriving as `Ok`. `sim-ecu` never sends 0x78, so the response-pending path through the worker is not covered end to end.
- A negative response is a value, not an error. A procedure that ignores it carries on, so the transpiler must emit the checks.
- Requests whose positive response is suppressed (the sub-function's suppress bit, for example tester present `3E 80`) end in `NoResponse`. A procedure cannot send them until the host knows which requests expect no answer.
- No CAN frame padding or addressing format ComParam is set. `sim-ecu` does not need them, but real ECUs may.
- A program that needs seed-key, HMI, records, flash or monitor capture cannot run until those backends exist. It fails at its first such primitive, with the state pointing at it.
