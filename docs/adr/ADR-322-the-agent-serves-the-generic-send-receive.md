# ADR-322: The Agent Serves the Generic Send-Receive

**Date:** 2026-10-17
**Status:** Accepted
**Affects:** `agent` (`send_receive`, `host.rs` `WorkerHost::send_receive`, `policy::check_program`,
`policy::writes`, `runner::check_job`, `Job::writes`, `ngr-agent run`), design 8.2.4;
ADR-235, ADR-245, ADR-247, ADR-250, ADR-257, ADR-309, ADR-321

## Context

ADR-321 item 5 had the agent refuse `Op::SendReceive` until its host could match answers against
the declarations. The instruction carries its whole request on the stack, so the service
identifier it sends is the first byte of a run-time value. Three checks were written for
instructions whose service the code shows:

- the policy (ADR-235 item 8, ADR-247), which refuses a request before the link opens and again
  once the VCI is known;
- `policy::writes`, which decides whether a job takes the device's reprogramming slot (ADR-257);
- `Program::validate`'s flash-plan rules (ADR-245, ADR-250): no download outside a recovery plan's
  transfer range, and no session change, reset, upload or file transfer inside a transfer.

If a send-receive's service were known only at run time, the first would run only in the host,
the second could not run at all (a write through a send-receive would not take the slot), and the
third would have nothing to check.

The instruction also names a declared service (`Service.id`), and the frontend builds the request
from that service's declared request, so the two start with the same byte.

## Decision

1. **A send-receive's service identifier is its declared service's.**
   - The host sends only a request that starts with the `Request.sid` the job's document declares
     for the service the instruction names. Another first byte is refused before anything is sent
     (`HostError::WrongService`), and so is an empty request (`HostError::EmptyRequest`).
   - The service must be declared with a request and a positive response. A program alone has no
     declarations, so its send-receives are refused (`HostError::UndeclaredService`).
   - The SID is therefore known before the program runs. `policy::check_program` takes the job's
     variant and checks a send-receive's declared SID as it checks a `ServiceRequest`'s, at both
     checkpoints. `policy::writes` uses the same check, so a send-receive that writes takes the
     reprogramming slot. The release agent's read-only rule holds unchanged: the list is UDS's
     until it is kept per application layer.
   - The host still checks the request it sends against the link's permission, as it does for
     every request.
2. **The flash-plan rules apply to the declared SID.**
   - RequestDownload and RequestTransferExit are refused in a send-receive wherever it stands. A
     recovery plan's declared boundaries are `ServiceRequest` instructions, so `Program::validate`
     would refuse either service anywhere a send-receive can be. TransferData goes through
     `FlashTransfer` (ADR-250), as for `ServiceRequest`. The policy refuses all three before the
     link opens, and the host refuses them again.
   - A session change, a reset, an upload or a file transfer is refused while the host tracks a
     transfer (ADR-250). `Program::validate` refuses these between a plan's RequestDownload and
     its transfer exit; the host's tracked transfer stands for that range at run time. Outside a
     transfer they pass, as they do in a `ServiceRequest`.
3. **The answer is matched against the declarations.**
   - Positive: the answer starts with the declared positive response's SID and prefix. The worker
     is asked for answers that start the same way.
   - Negative: the negative-response shape of the variant's protocols (ADR-321 item 4). Every
     protocol must declare one, all must be the same, and the code must follow the marker and any
     echoed service identifier.
   - The workers recognise only the `7F` marker, the echoed SID and the code at the next byte, with
     the pending codes 0x78, 0x21 and 0x23 (ADR-309 item 5). Any other shape is refused. The
     document comes from the server and is not trusted to be consistent.
   - The host skips a negative answer whose code is a declared pending code, as it skips 0x78 for
     the other instructions (ADR-235 item 3).
   - `check_job` checks the shape before the link opens, so an unusable one fails before
     anything is sent.

## Consequences

- A procedure with `diag.read_dtc` runs on the agent. An end-to-end test runs one through
  `j2534-0404-service` against `sim-vci`.
- `check_job` checks the declarations before the policy, so a job with a send-receive and no
  document is refused as having no declarations.
- The read-only list is still UDS's. Keying it by the application layer the link's protocol
  declares, and no longer building request bytes for `ReadDtc`, remain open (ADR-309 item 4).
- A definition whose request SID differs from the first byte the frontend builds cannot occur, since
  the frontend builds the request from the declared one. A hand-built document that tried it is
  refused at the instruction.
