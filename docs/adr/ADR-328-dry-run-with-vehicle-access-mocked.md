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
   equally well, a `ServiceRequest` operand wider than a byte, a `SendReceive` to a service
   declared without a request and a positive response, or a `SendReceive` request that is empty
   or starts with another SID (the agent's host refuses the last two, ADR-322 items 1 and 3)
   ends the run with a host error: a definition that sends what it does not declare is what the
   validation is for.

3. **Answers come from the script, else from the declaration.** The script holds whole
   responses per service short name, negative responses included. Without one, the answer is
   built from the service's declared positive response: its SID and prefix, then the fewest
   bytes each field decodes from, all zero but a terminator. A fixed field takes its width; a
   leading length is a zero prefix and an empty value; a length or repeat count taken from
   another field is that field's zero, so an empty bytes or text value or no items (a number
   cannot be zero bits wide, so one with such a length ends the answer); a terminated value takes
   its minimum bytes and then its terminator, or its maximum when no terminator fits below it; a
   group takes its layout's declared size and a fixed repeat its items' total size. A value
   that reads to the end of the message (a terminated value with no terminator and no maximum,
   a repeat to the end) ends the answer, and so does a field whose extent the declaration does
   not give (a select, a repeat up to a marker, a layout of no declared size): a field placed
   after it has no bytes, and since a response decodes whole or not at all, every field read of
   it fails, so such a response needs a scripted answer. A length or count field with a
   conversion that maps zero elsewhere needs one too. Otherwise a built answer lets a procedure
   go on and its field reads decode; a procedure that
   checks a value may then take its own failure path, which the report shows as the run's end.
   A service with no positive response needs a scripted answer.

4. **Every answer is decoded and reported.** Each request event carries the request, the
   service it went to, the answer, whether it was built or scripted, and the answer decoded
   through the service's positive response (`decode_positive_response`, ADR-297), so a negative
   or malformed answer shows as a decode error in the report rather than stopping the run. The
   run stops only where the VM stops (a VM error, `Fail`, the end), the host cannot answer, or a
   limit is reached (item 7). A scripted negative response is the final answer: a response
   pending code, which the worker absorbs on a real link, reaches the procedure as it is.

5. **Lists answer in order, then repeat their last answer.** Each service's responses, and the
   keys, HMI answers and record answers, are used in order; once a list is used up its last
   answer is given again, so a procedure that polls a routine gets a steady answer instead of
   failing on the poll after the scripted ones.

6. **Nothing waits.** `Wait` passes at once and is reported with its duration. `SecurityAccess`,
   `HmiRequest` and `RecordInput` take the next scripted key or answer; with none scripted the
   run ends with a host error rather than waiting, because a dry run has no server or operator
   to wait for. `FlashTransfer` and `MonitorCapture` are reported and succeed. Log messages are
   reported.

7. **Bounded by the caller and by the report.** The caller gives a step limit. The report holds
   at most `MAX_REPORT_BYTES` (64 MiB) of requests, answers, service names, decoded values
   (each field's size, name and bytes, nested fields included), seeds and keys, transfer data,
   forms and messages, each event also counted as `EVENT_COST`, twice its size in the events list, whose storage can
   grow to twice its length; an
   event past that ends the run with a host error before it is stored, a log message past
   it, which the host cannot refuse, ends the run after its step, and a host error that ends
   the run keeps only the first `MAX_ERROR_REQUEST_BYTES` (64) bytes of the request it names. A definition run
   at ingestion is untrusted, and a loop resending a 16 MiB value would otherwise grow the
   report without bound.

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
- Only what the declaration decides is checked. The agent's own refusals at run time (the
  flash-plan rules for a send-receive, ADR-322 item 2, and a `TransferData` sent by
  `ServiceRequest` instead of `FlashTransfer`, ADR-250) are not repeated, so a run the dry run
  finishes can still be refused by the agent there.
- When the IR gains targets (ADR-325), the dry run takes the declaration part of each target
  instead of one variant.
