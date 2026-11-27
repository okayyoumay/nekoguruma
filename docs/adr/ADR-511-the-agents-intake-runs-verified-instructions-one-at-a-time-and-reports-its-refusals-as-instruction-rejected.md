# ADR-511: The Agent's Intake Runs Verified Instructions One at a Time and Reports Its Refusals as Instruction Rejected

**Date:** 2026-11-27
**Status:** Accepted
**Affects:** `crates/agent` (the intake, `runner`, the VCI shell shared with `ngr-agent run` and `ngr-agent identify`, `connect`), `crates/server/src/job_state.rs`, `api/asyncapi.yaml`, `docs/system-architecture.md` 5.3, 5.6 and 8.2.6, `crates/agent/docs/ngr-agent.md`

## Context

The control channel's gate verifies an instruction, records the job in the received-jobs
ledger (ADR-489), hands it to an `Intake` and acknowledges it (ADR-473, ADR-478 item 5). The
intake given to it so far logs the job and drops it, so a verified instruction starts nothing.

Starting the job leaves several outcomes that no earlier decision gives:
- what the agent reports when it refuses an instruction it has already acknowledged;
- what it reports when it cannot fetch what the instruction names;
- what happens to an instruction that arrives while a job runs;
- what is recorded so that an end is reported once.

The local `ngr-agent run` path also holds the shell that finds the VCI, takes its locks and
launches its worker in the binary, out of reach of a second caller.

## Decision

1. **One run at a time.** The intake runs one job at a time per agent. Instructions taken while
   a job is in progress wait in the order they were taken.
   - `take` only queues the job and returns, so the gate's promptness rule holds.
   - The queue has no bound, so `take` never fails for load.
2. **The order of the checks.** A job is checked in this order, and each refusal comes before
   anything that needs the next step:
   1. the instruction's own shape, before any fetch;
   2. the fetch;
   3. the documents held against the documents named;
   4. the kind against the document;
   5. the build's policy;
   6. the locks, the worker and the link.
3. **The intake's refusals.** The intake ends the job as `failed` with the failure code
   `instructionRejected` when:
   - a non-identification instruction names other than one document;
   - an identification instruction has no plan hash, or its plan's SHA-256 differs from that
     hash;
   - an `acquire`, `monitorSession` or `monitorCapture` instruction names a document that writes
     (`policy::writes`, which counts a program as writing unless it is provably read-only);
   - the server refused the object with a proven answer (ADR-504 item 3).

   Which check refused is written only to the agent's log. The server stores the agent's
   `instructionRejected` as it is; its own `instructionKeyRetired` stays the server's only
   failure code (this supersedes the agent-side restriction of design 5.6 and `asyncapi.yaml`).
4. **Other failures before the run:**
   - a document or plan that does not load: `preconditionFailed` with `jobRefused` (ADR-475 item 2);
   - a fault of the agent's own, or a held set that is not the named set: `preconditionFailed` with `internal`;
   - a VCI that the registration definitions do not name, that both the J2534 and the D-PDU API definitions name (no API is preferred), or whose worker does not launch: `preconditionFailed` with `linkNotOpened`. A definition that names the VCI ambiguously or with an invalid entry still counts as naming it, and a definition that cannot be read fails the lookup with its cause, again `linkNotOpened`.
5. **An unavailable server is waited for, until the start deadline.**
   - A fetch that runs out of attempts, including an answer that mismatches twice, is the server unavailable, never a refusal (ADR-504 item 3).
   - The intake waits as ADR-504 item 7 does, at most 60 seconds, and fetches again.
   - The start deadline is judged:
     - when a job leaves the queue;
     - after each failed round;
     - during a fetch in flight;
     - during the wait for the locks;
     - between the shell's launch of the worker and the job's first step.
   - A job whose deadline passes ends as `expired`, and the next job starts.
   - The last judgement is made at the first step, by the hook that item 6 describes, which judges the start deadline itself, with the issue-time mark and the clock in memory (a poll of the watching task, once a second, would let a first step just after the deadline begin), and refuses the job when it has passed, and otherwise lets it begin: the deadline and the hook each take one shared state out of "waiting" by a compare-and-swap, so exactly one of them wins and the deadline cannot cut a job that has begun. A job refused there sends nothing to the ECU and ends `expired`, with no failure code and no detail. (Amendment by the maintainer, 2026-11-27: before it, the deadline stopped a job only until the shell launched the worker.)
6. **What is reported, and when.**
   - `running` is sent once, from a hook that the journaled run and the identification run call after their pre-run checks, before the first instruction. The hook decides whether the job begins (item 5) and sends `running` only when it does.
   - A run's end is mapped by `job_report::outcome` (ADR-475).
   - While the agent stops, nothing terminal is reported, except a run that ended `Ok`: its `completed` is reported and recorded, so a finished write is not lost.
7. **The outcome record.** A per-agent append-only file beside the received-jobs ledger holds:
   - `ended` (the job, its generation and its terminal state), synced before the event is appended to the outbox;
   - `reported`, written after that append.

   A start reads it, so an end is appended to the outbox at most once more after a crash between the two.
8. **The result.**
   - Only a run that ended `Ok` enqueues its result in the upload ledger: its final VM state, or an identification's report.
   - A failed run enqueues none.
   - The communication log is written at `comm_log_path` under `journal_dir(mode)`. It is uploaded only on an operator's request (ADR-455 item 1, ADR-508); the intake does not enqueue it.
9. **The VCI shell is in the library.** Finding the instruction's VCI by its name in the registration definitions, taking its locks and launching its worker is one function, used by the intake and by `ngr-agent run` and `ngr-agent identify`. Its wait for the locks can be cancelled. Tests replace the lookup.
10. **Identification has a plan source.** The server serves no identification plan yet, so the production plan source has none, and an identification instruction is refused before any fetch. Tests supply a matching and a differing plan, so the plan-hash check is exercised.
11. **A job is served after its `received` event is appended.** The control channel appends the
    job's `received` event after `take` returns (ADR-489, ADR-478 item 5), and a refusal needs no
    time, so an intake that served a job at once could append a terminal state before
    `received`. `Intake` therefore has a second method, `received(job_id, generation)`, which the
    channel calls once that append has returned (not when it failed or the stop came first), and
    the intake moves the job from the jobs it has taken to the queue it serves only then. A job
    whose `received` is never appended stays taken and unserved until the resume item starts it.
12. **The lock directory is in the mode's state directory.** (Decision by the maintainer,
    2026-11-27.) The default lock directory of `connect`, `ngr-agent run` and `ngr-agent
    identify` is `locks` in the state directory of the agent's mode, the directory that holds
    its journal; `--locks` still overrides it. It replaces the `locks` directory next to the
    executable (ADR-257 item 4), which an agent that runs as an unprivileged account, as under
    the packaged service, cannot create, so that every job would end `preconditionFailed`. The
    agent makes the directory as it makes the journal's, private to its account. Agents of one
    mode therefore share their locks with no flag; a device-mode service and a user-mode run
    have different state directories and do not share them, unless both name one directory with
    `--locks`. The worker builds stay next to the executable, since the agent only reads them.

## Rejected

- **Running jobs in parallel when their locks allow.** Faster with several VCIs, but every case of the queue would also depend on which locks are free. The one-at-a-time queue is simpler and has no VCI contention.
- **Refusing an instruction while a job runs.** The server would redeliver it at least once (ADR-476), so the agent would see the same job again only to refuse it again.
- **A new failure code for the server being unavailable.** An unavailable server is a delay, not an end, until the start deadline passes; `expired` already names that end.
- **`preconditionFailed` with `jobRefused` for the intake's refusals.** That would not tell a refusal of the instruction from a refusal for the vehicle's state.

## Consequences

- A queued job that waits behind a long run, or behind an unavailable server, may expire before it starts. The server's own expiry usually comes first (ADR-487).
- Until the server serves plans, identification jobs over the control channel are refused.
- Resuming a held job after a restart, reconciliation after a reconnect, and an operator's cancel are not part of this decision.
