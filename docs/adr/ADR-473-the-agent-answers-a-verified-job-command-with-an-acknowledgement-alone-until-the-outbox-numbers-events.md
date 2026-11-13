# ADR-473: The Agent Answers a Verified Job Command With an Acknowledgement Alone Until the Outbox Numbers Events

**Date:** 2026-11-13
**Status:** Accepted
**Affects:** design 5.3; `api/asyncapi.yaml` (`agentControl`); `crates/agent` (`control`, `instruction`), `crates/agent/docs/ngr-agent.md`, `crates/agent/docs/operational-log.md`; details ADR-466 items 10 and 13 and ADR-461 items 9 and 10 for the agent's side, and ADR-471 item 5's job command

## Context

ADR-471 left a job command decoded and unanswered until a gate and an intake existed. The plan
for this step was to answer a verified command with `commandAck` and a `jobState` event for
`received`. An `agentEvent` carries a `seq` that the server will deduplicate by, together with
the agent ID, and that a cumulative `eventAck` will confirm (design 5.3, ADR-466 item 10). No
outbox numbers events yet: a counter that starts again at every process start would give a
later event the number of an earlier one, which the server would then drop as a duplicate. A
number that a cumulative acknowledgement can confirm must also leave no gaps, or the server
cannot tell a lost event from a number never used; an identifier that is only unique, such as
a UUIDv7, needs an acknowledgement per event instead, and its order rests on the device clock.

## Decision

1. **A verified command gets `commandAck` alone.** No `agentEvent` is sent for it. The
   `received` event, and how events are numbered and acknowledged (a persisted counter with no
   gaps and a cumulative acknowledgement, or a unique identifier acknowledged per event), are
   decided by the outbox work.
2. **One gate per connection.** Each `jobCommand` goes through the checks of
   `agent::instruction::open`, in the same order, with the trusted key and the agent ID of the
   registration record. A refusal is answered with
   `commandRejected` and its coarse reason on the connection the command came on (ADR-466 item
   13); a command whose `signedJob` is missing or not a string is refused as `malformed`. A `commandId`
   too long for its answer to fit in a message is not usable: the connection closes with 1002, as
   for a command without one, and the intake sees nothing.
3. **The gate bounds the refusal log.** The first refusal of each reason on a connection is a
   warning and later ones are debug lines; when a connection that refused anything ends, one line gives the
   count of every reason. The gate's refusals are not logged a second time by `open`, which
   keeps its own entry for one-shot callers.
4. **A verified instruction goes to an intake that the caller gives the session.** The intake
   is a trait passed to `Opened::start`; its first implementation records the verified job ID at
   debug level and drops the instruction. The acknowledgement follows the hand-over, and the
   duplicate check and the journal record of design 5.3 belong to the intake. An
   acknowledgement that is lost after the hand-over makes the server deliver the job again, so
   an intake must expect one job twice and be idempotent on its job ID.
5. **The status counts answers.** `commands_acknowledged` and `commands_rejected` replace the
   count of unanswered commands; both count across connections.

## Consequences

- The server's job queue settles a delivery by `commandAck` and gets no `received` state from
  the agent until the outbox exists; the job's state stays the server's to show.
- An acknowledged job is not run while the intake is not connected to a runner.
- The intake's form fits a hand-over that cannot fail and does not wait; one that writes a
  journal record before the acknowledgement, and withholds it when the record fails, needs the
  form to change, which no caller outside the agent's tests depends on.
- The outbox work must decide the numbering and acknowledgement scheme, and write into
  `api/asyncapi.yaml` whether `seq` may have gaps.
