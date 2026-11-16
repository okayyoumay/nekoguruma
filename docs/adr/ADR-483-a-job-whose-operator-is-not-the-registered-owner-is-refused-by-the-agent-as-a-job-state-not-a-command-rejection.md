# ADR-483: A Job Whose Operator Is Not the Registered Owner Is Refused by the Agent as a Job State, Not a Command Rejection

**Date:** 2026-11-16
**Status:** Accepted
**Affects:** design 5.3, 5.6, 6.6; `api/asyncapi.yaml` (`failureCode`, `ackCommand`); `crates/agent` (`control::gate`, `control::channel`), `crates/agent/docs/ngr-agent.md`, `crates/agent/docs/operational-log.md`; `crates/shared-proto` (`FailureCode`); `docs/glossary.md` (Owner); details ADR-466 item 13, ADR-461 item 10 and ADR-478 item 5

## Context

A signed job instruction names the operator who created the job (ADR-461 item 4), and an agent is registered to one owner (ADR-463). Nothing on the agent compared the two. The server's permission test decides who may create a job for an agent, but design 6.6 gives the agent its own restriction preset, so an agent that is `personal` should not run a job made out for another operator of its tenant even if the server's list or its authorization is wrong.

ADR-466 item 13 says a `commandRejected` settles the delivery and ends the job as failed by the server's own decision, and that expiry and the operator match are job states, not rejection reasons. ADR-461 item 10 fixes the closed set of rejection reasons for an instruction that fails verification. An instruction whose operator is not allowed has verified: it is the server's own signed word. A job-scoped state exists for it (ADR-461 item 10), and the outbox is where a job's must-deliver states go (ADR-478).

## Decision

1. **The check is a step of the control channel's gate, after verification.** It runs only on an instruction that passed every check of the gate (ADR-461 item 9, the agent ID included), before the intake takes it and before anything is appended to the outbox. An instruction for another agent or one that does not verify never reaches it, and is answered as before, whatever operator it names.
2. **The rule.** The instruction's operator ID must equal the registration record's owner ID as exact bytes: no case folding, no trimming. The policy is derived once per session from the record's preset with an exhaustive match on the preset type: `personal` and `personalRemote` require the match, and `shared` checks nothing. A preset that does not parse is owner-only (the record's reader refuses one, so this is not reached).
3. **A mismatch is a job state, not a rejection.** The instruction is not handed to the intake. Instead of the `received` event the channel appends one terminal `jobState` event, `preconditionFailed` with the new failure code `operatorNotAllowed` and no detail, under the same admission bound and with the same failure handling as the `received` append (ADR-478 items 4 and 5): a full, failed or poisoned outbox withholds `commandAck`, and a poisoned or failed write closes the connection with 1011. The channel then answers `commandAck`. This keeps ADR-466 item 13 as written: the rejection reasons stay the five of ADR-461 item 10.
4. **`commandAck` means the delivery is settled, not that the job was accepted.** The server marks the job delivered when the terminal event is stored, ends it, and frees its vehicle lock (ADR-481). A refused job is never journaled and never reaches the intake; a redelivery of it gets a second terminal event, as a redelivered `received` does today.
5. **`operatorNotAllowed` is an agent's own code.** The server's report handling keeps it for `failed` and `preconditionFailed` as it keeps the other agent codes (ADR-481); the server's logic does not change.
6. **The `shared` preset has no agent-side check.** The server's permission test stands alone there: the agent holds no allowed-operator list. Nothing reads the registration's remote flag, since the signed instruction carries no remote flag (ADR-461 item 4 leaves it out of version 1).
7. **The log is bounded and names neither identity.** The first refusal per connection is a `warn`, the repeats are `debug`, and the end of the connection counts them. An entry holds the job ID and the code, never the operator ID, the owner ID or a `commandId`.
8. **The match binds a server that is wrong, not one that is compromised.** The operator field is part of what the instruction key signs, so whoever holds that key chooses it. The check protects an agent against a server whose allowed-operator list or authorization is wrong but which signs the true creator.

## Consequences

- A refused job costs the server one event and one acknowledgement, and the agent keeps nothing of it after the outbox is acknowledged.
- The start-deadline check (expiry) reuses this path and runs after this one: an instruction that is both for the wrong operator and expired ends as `operatorNotAllowed`.
- The idempotent-command work checks acceptance again when a journaled job resumes, since the record's preset or owner can change between delivery and resume.
- The server's `agent_allowed_operators` is the owner alone today. A later edit of it must refuse a second operator on an agent whose preset is `personal` or `personalRemote`, or the server would deliver jobs the agent refuses.
- A new failure code reaches `api/asyncapi.yaml` and the glossary; an agent older than this change never sends it, and the server treats an unknown code as `other`.
