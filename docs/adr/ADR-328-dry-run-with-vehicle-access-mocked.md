# ADR-328: Dry Run with Vehicle Access Mocked

**Date:** 2026-10-18
**Status:** Accepted
**Affects:** `diag-ir` (`dry_run`: `Script`, `dry_run`, `DryRunReport`), `diag-frontend` tests
(`tests/dry_run.rs`), design 12.1, `crates/diag-frontend/docs/starlark-subset.md` 6; ADR-233,
ADR-280, ADR-297, ADR-321

## Context

Design 12.1 has the server validate a procedure definition at ingestion by running it on the
`diag-ir` VM with vehicle access mocked, and the Starlark subset asks for the same check after
generation. Until now the only hosts that answered without a vehicle were test doubles: the
golden test's recording host, the VM's unit-test host and the agent runner's fake worker. Each
answered by its own rule, none was public, and none reported what it was asked in a form another
caller could check.

A shared dry run has to settle three things the VM leaves to its host:

- which declared service a request belongs to, since `ServiceRequest`, `ReadDtc` and
  `RoutineControl` carry no service id, only a SID or a UDS operand;
- what to answer when the caller has not said, since a server validating at ingestion has no
  recorded responses;
- what the primitives that wait on the outside world (`Wait`, `SecurityAccess`, `HmiRequest`,
  `RecordInput`) do when nothing real is behind them.

## Decision

1. **A public dry run in `diag-ir`.** `diag_ir::dry_run::dry_run` takes a `Program`, the
   declaration part of its variant, a `Script` and a step limit, and returns a `DryRunReport`:
   the events in order and how the run ended. It lives beside the VM, because the server, the
   frontend and tests all run the VM through `diag-ir` and none of them should depend on another
   for it. The program is first checked with `Program::validate`; a refused program ends the
   run before it starts.

2. **A request is resolved to a declared service.** `SendReceive` names its service by id. Every
   other request is rebuilt as the whole request the agent sends (the SID and operand of a
   `ServiceRequest`; `ReadDtc` and `RoutineControl` as the agent's host builds them for UDS) and
   resolved to the declared service with the request's SID whose request prefix is the longest
   the request starts with. A request no declared service matches, two services matching it
   equally well, or a `SendReceive` to an undeclared service id, ends the run with a host error:
   a definition that sends what it does not declare is what the validation is for.

3. **Answers come from the script, else from the declaration.** The script holds whole
   responses per service short name, negative responses included. Without one, the answer is
   built from the service's declared positive response: its SID and prefix, then zero bytes to
   the end of the last field whose position and length are fixed, so a field of another length,
   a shaped field, and any field placed after one of them are left empty. A built answer is
   enough for a procedure to go on and for its field reads to decode; a procedure that checks a
   value may then take its own failure path, which the report shows as the run's end. A service
   with no positive response needs a scripted answer.

4. **Every answer is decoded and reported.** Each request event carries the request, the
   service it went to, the answer, whether it was built or scripted, and the answer decoded
   through the service's positive response (`decode_positive_response`, ADR-297), so a negative
   or malformed answer shows as a decode error in the report rather than stopping the run. The
   run stops only where the VM stops (a VM error, `Fail`, the end) or the host cannot answer.

5. **Lists answer in order, then repeat their last answer.** Each service's responses, and the
   keys, HMI answers and record answers, are used in order; once a list is used up its last
   answer is given again, so a procedure that polls a routine gets a steady answer instead of
   failing on the poll after the scripted ones.

6. **Nothing waits.** `Wait` passes at once and is reported with its duration. `SecurityAccess`,
   `HmiRequest` and `RecordInput` take the next scripted key or answer; with none scripted the
   run ends with a host error rather than waiting, because a dry run has no server or operator
   to wait for. `FlashTransfer` and `MonitorCapture` are reported and succeed. Log messages are
   reported.

## Consequences

- The server's ingestion check, the frontend and tests can share one host whose rules are
  written down. What an ingestion check counts as a pass (a procedure ending in its own failure
  on built answers is still a valid definition; a VM error or an undeclared request is not) is
  for the ingestion path to decide when it is built.
- The UDS bytes of `ReadDtc` and `RoutineControl` are written twice, in the agent's host and in
  the dry run, because `diag-ir` cannot depend on the agent. A change to one must change the
  other.
- Built answers are zeros, so they exercise a procedure's structure, not its values. Checking
  values needs a script.
- When the IR gains targets (ADR-325), the dry run takes the declaration part of each target
  instead of one variant.
