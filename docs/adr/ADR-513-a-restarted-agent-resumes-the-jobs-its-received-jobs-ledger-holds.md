# ADR-513: A Restarted Agent Resumes the Jobs Its Received-Jobs Ledger Holds

**Date:** 2026-11-28
**Status:** Accepted
**Affects:** design 5.3, 5.6, 6.7, 8.2.5; `api/asyncapi.yaml` (`failureCode`); `crates/agent` (`received_jobs`, `intake`, `connect`, `runner`, `journal`, `journaling`, `operator_policy`, `control::gate`, the resume-attempts file), `crates/shared-proto` (`FailureCode`), `crates/server/src/job_state.rs`, `crates/agent/docs/ngr-agent.md`, `crates/agent/docs/operational-log.md`; amends ADR-511 items 4, 5, 6 and 11 and its last Consequence, ADR-489's Consequences on the missing `received` and on the unrun recorded job, and ADR-475 item 2; agrees with ADR-229 item 2 step 1 and fulfils ADR-486's resume Consequence

## Context

The received-jobs ledger (ADR-489) records every job the agent accepts, with the signed envelope, before the job is handed to the intake. A recorded job whose hand-over never happened (an intake error, a stop or a crash after the record, a failed `received` append, a record that counted at a reopen after a failed write) is a duplicate when the server redelivers it, so nothing takes it again. A job that was handed over and was running when the agent stopped is in the same state: nothing starts it. ADR-511 item 11 and ADR-489 left both for a later decision, and ADR-486 left the re-verification and the deadline at a resume without a caller.

The decision has to answer which jobs are resumed and in what order, what is checked again and how a refusal ends the job, which jobs the start deadline still judges, what the first step reports, and how often a resume that cannot make progress is retried.

## Decision

The agent resumes the jobs it holds itself; the workers and the server change nothing (design principle 6).

1. **The held set and its order.** A job is held when the ledger's record of its highest generation has no `ended` record in the outcome record. The agent opens the outcome record before the ledger, and the ledger's open returns the held records, each job's highest-generation record with its envelope, in the order those records lie in the file. At connect the held jobs go to the front of the intake's queue, in that order, before the control channel opens; a job delivered later queues behind them (ADR-511 item 1). The held predicate is checked again at each dequeue.
2. **A superseded generation is dropped.** A held `(J, g)` that the ledger has since superseded by `(J, g+1)` is dropped at its dequeue, with no event and no `ended` record.
3. **The order of the checks at dequeue.**
   1. Is the job still held (item 1)?
   2. Verify the recorded envelope again (item 5).
   3. Raise the issue-time mark and sync it when it is behind (ADR-486).
   4. Check the acceptance (item 6).
   5. Probe the journal (item 4).
   6. Judge the start deadline where item 9 says it is judged.
   7. Check the attempt bound (item 14).
   8. Append `received`, when the job has not begun (item 4), with the kind `Received` and the admission bound as the channel uses it.
   9. Write the attempt record (item 14).
   10. The checks of ADR-511 item 2: shape, fetch, documents, preparation, the standing with the program, the shell and the run.

   Every ending this decision adds goes through the intake's one `finish`, so the outcome record, the outbox and the vehicle lock follow the existing rules.
4. **"Begun".** A job has begun when its journal holds a run start, which the run commits after the first-step hook said Go, or holds progress that only a run makes: a completed step, an intent, a transfer or a first request. The second clause covers a journal written before run starts existed (ADR-272), which has progress and no run start. A journal that exists but holds neither (a creation-only journal, which a crash between the creation and the run start leaves, or one that holds only a resume) is not begun; neither is a missing journal. A job that has not begun is a first run in the sense of ADR-475 item 2, whatever the files hold; this replaces the reading "no journal means first run". `resume_program_journaled` takes the first-step hook and calls it before it resumes.
5. **Re-verification.** The recorded envelope is opened again with the registration record's instruction key (`instruction::open`); nothing re-encodes the instruction, and the verified instruction's job ID and ownership generation must equal the record's; a mismatch ends like a failed verification. A record that no longer verifies ends by the standing of item 7 with the failure code `instructionNoLongerVerifies`, which is added to `shared-proto`'s `FailureCode`, to `api/asyncapi.yaml` and to design 5.6. The detail is empty and the reason goes to the agent's log only. The server keeps the code as the agent's own (a server that predates it stores `other`).
6. **Acceptance.** The instruction's operator is checked against the registered owner with the preset the record has at that time (design 6.6, ADR-483). The check is the gate's own, `operator_policy`, so the two cannot differ. A refusal ends by the standing of item 7 with `operatorNotAllowed`.
7. **How a refusal at dequeue ends: by the journal's standing.** For a verification or acceptance refusal: a job not begun ends `preconditionFailed` with the code; a begun job whose journal is Clean ends `failed` with the code; a Decided journal ends with the journal's verdict; an OnSite(reason) journal ends `needsOnSiteRecovery` with the reason; a Resumable journal, or a standing that is not known, ends `needsOnSiteRecovery` with a fixed-word detail. A refusal before a resume-mode run ends the same way (item 11).
8. **A program-free standing.** The standing needs the program in general. Without it the agent reads: Decided and unreadable are known; Clean when the journal holds no completed step, intent, transfer or standing request; anything else is unknown until the program loads, and counts as not clean. A journal that is not begun (item 4) holds none of those, so it is never unknown: it is Clean. No case is left: a standing request comes from a request record, which sets the first request that `begun` counts. A Decided journal gives its verdict at once. A corrupt journal, another job's journal or an unsupported version ends `needsOnSiteRecovery` with the reason `UnreadableJournal` at once. An I/O error other than not-found leaves the job open: it is queued again and counted (item 14).
9. **The deadline rule.** The start deadline is judged wherever the program starts from its first instruction: no journal, a journal that has not begun, and a begun journal that is Clean. Such a job past its deadline ends `expired` (with the raise and judgement of ADR-486 item 4). A continuation from the journal (Resumable) is not judged; the restart order's identity checks guard it. This agrees with design 8.2.5, the diagram of 5.6, ADR-229 item 2 step 1 and ADR-486's resume Consequence: a job whose run continues is never expired, a run that restarts is. A job whose deadline is not judged gets one fetch round and a 60-second wait for the locks, then ends open and is queued again (item 13).
10. **`running` at every begun run.** The first-step hook appends `running` every time a run begins, the first run and every resume, so the server's state follows an agent that restarted; a repeat only touches the server row (design 5.6). This amends ADR-511 item 6, which sent it once.
11. **Pre-run classes.** A failure before the run is permanent or transient. Permanent: the instruction's shape; a fetch refused with a proof; a document or plan that does not load; a held set that is not the named set; the kind against the document; errors of `check_job` and `repeats_on_every_resume`; a VCI that is unknown or ambiguous. Transient: a fetch answered Unavailable or failed locally; an artifact missing in the cache; a journal or log directory that is unusable; definitions that cannot be read; a worker that does not launch; the locks; any other `JobError`. A permanent failure ends by the standing of item 7; a transient one leaves the job open, queued again and counted (items 13 and 14). A job that has not begun keeps ADR-511's endings for a first run.
12. **What `recover()` keeps doing.** The restart's `recover()` keeps its reads and its identity checks, and the resume pass starts the run through it; the pass does not decide what the journal allows.
13. **The 60-second re-queue and bounded waits.** A job whose end is open (a transient failure, a stop that did not end it, a run that ended open) is queued again at the end of the queue after 60 seconds, within the same process, while the agent is not stopping, including a job that ends open in the process that first took it, at the one place where the intake's `serve` handles it. The waits of the pass are bounded: one fetch round, then a 60-second wait for the locks, then the job ends open.
14. **The attempt file and its bound.** A run that ends open with no change of its baseline, that is, no journal then a journal, or growth of the journal's resume total, counts as an attempt. The fifth consecutive attempt ends the job by the standing of item 7 with the failure code `internal`; a deadline that has passed for a job that has not begun wins, and it ends `expired`. The count lives in `<agent-id>.resume-attempts.v1` beside the ledger, a file of the shared frame layout with its own lock, opened in `connect` like its siblings. An attempt record is written and synced before the run starts, so a job that crashes the agent is counted. The count is the trailing records that share the current baseline; there is no reset record. A queue-again before the lock reuses the attempt record in the same process and is not counted again.
15. **The agent holds the pass out of a stop.** A job is not started while the agent stops; its end stays open and it is held again at the next start.

The failure code `instructionNoLongerVerifies`, the ledger's open that returns the held records, the acceptance check shared with the gate and the journal's `resume_total` and `begun` accessors are the building blocks of the items above.

## Rejected

- **"A journal exists" as begun.** A creation-only journal would be taken for a run that started, so a job that never began would escape the start deadline and be reported as a failed run.
- **`preconditionFailed` for every re-check refusal.** A begun job that is Clean has sent nothing yet but has started; its end is `failed`, and a Decided or on-site journal must keep its verdict.
- **The deadline for every resume.** It would expire a job whose run continues, which design 8.2.5 never does.
- **No deadline for any journal.** A clean journal restarts the program from the beginning, so a stale job would run.
- **A second outcome-record version for the count.** It needs a migration of the file for one number; a file of its own does not.
- **A reset record in the attempt file.** The count is the trailing records of the current baseline; a reset adds a record whose absence would be a bug.
- **`u64::MAX` as "no deadline".** A sentinel that every later reader would have to know.
- **Deleting a creation-only journal and running as a first run.** The journal is the job's own file, and a delete is a loss the file does not warrant.
- **Appending `running` from the intake before the shell.** The job has not begun then, and the first-step hook is where item 9 decides it.

## Consequences

- **A job that becomes held while the process keeps running runs at the next start.** A ledger frame that counts at a reopen after a failed write, and a job whose `take` fails after its record, are held jobs, and the pass reads the ledger at connect only.
- **A crash between the hook's `running` append and the run start's commit leaves the server at `running` while the agent reads the job as not begun.** The next start may then end it `expired` or `preconditionFailed` from `running`.
- **Risk: the channel's operator refusal for a recorded job writes a held job's end outside the outcome record.** ADR-489's order checks the operator first, so a redelivery of a held job by an operator who is not allowed gets the terminal refusal and leaves the record. For a begun job it can replace the resume's ending on the server; that needs a changed record and a redelivery.
- **About five minutes of an unreachable VCI end an interrupted write job `needsOnSiteRecovery`** with no later retry: five attempts one minute apart (item 14).
- **A clean stop in the middle of an attempt counts** as an attempt.
- **A job that waits for the server or for locks loops every 60 seconds without bound** while it sends nothing (item 13).
- **A re-registration abandons the old agent ID's held jobs:** the new agent ID starts with an empty ledger (ADR-489).
- **A lost journal reads as never begun,** so the job runs as a first run and is judged by its deadline.
- **The finished-run window:** between a run's end and its `ended` record a Clean re-run past the deadline ends `expired`.
- **Deploy the server before the agents:** an older server stores the new failure code as `other`.
- ADR-511's last Consequence, that resuming a held job is not part of its decision, is answered by this one; ADR-489's two Consequences on the missing `received` and on the unrun recorded job are answered by items 1 to 4.
