> **TEMPORARY WORKING MATERIAL.** This file is consumable: it is the draft for the HMI protocol ADR named in `backlog.md`'s item on the agent's HMI support, and is deleted when that ADR is written. Permanent files must not reference it. See `work/README.md`.

# Draft: the agent-server HMI protocol

ADR-340 fixes the HMI form encoding, the answer and the lowering, and leaves the messages between agent and server, and how inquiries, records and the timeout survive a restart, a replay from the start and a handover, to an ADR of their own (ADR-340 items 5 and 6). The text below is what ADR-340's review settled before that split: a starting point for that ADR, not a decision. Each paragraph answers a review finding; check it against the agent's and the server's design when the ADR is written.

Settled points, in short:

- A request id per host instance, so a restart asks again; every message carries the ownership generation (ADR-229) and the agent drops a command that names no open inquiry.
- Messages: `HmiRequest`, `HmiRunStart`, `HmiWithdrawn` (agent to server); `HmiAccepted`, `HmiTimedOut`, `HmiAnswer`, `HmiRefused` (server to agent). An `Int` parameter is a decimal string in JSON.
- Records: before each run the agent sends the inquiry number it starts from, and the server supersedes the job's records at or above it.
- Timeout: the server times the job's wait on its own clock and reports it; the agent times it on its monotonic clock only while the server cannot be reached; `Record::HmiTimedOut` is journaled before the cancel is raised and is never cleared; at a handover the server marks the job timed out from its own wait or a reported withdrawal; the server's wait is the timeout of design 5.6.
- Withdrawal of every inquiry that ends without an answer, before the job's final state is reported.

The draft text, as it stood in ADR-340:

5. **The agent's host and the server.** The host keeps, per instance and in memory, a map from
   inquiry number to the request's state, as `wait_elapsed` keeps its timers: the first poll of an
   inquiry generates a `request_id` (UUIDv7, design 4.4), decodes the `HmiForm`, sends
   `EventBody::HmiRequest { job_id, generation, request_id, inquiry, kind, form }`, with
   `inquiry` the VM's inquiry number (the server answers with `HmiAccepted`, and later `HmiTimedOut` if its wait expires, item 6), `kind` `Form` or
   `Record`, `generation` the job's ownership generation (ADR-229: every job-scoped message
   carries it, and the server applies one only under the current owner and generation), and
   `form` typed (`id` and `params`, which serialize as JSON on the wire, an `Int` as a
   decimal string so that no value outside the range a browser holds exactly is rounded before
   the operator sees it), and returns `Ok(None)`;
   later polls return `Ok(None)` until the answer command `HmiAnswer { job_id, generation,
   request_id, ok, value }` arrives, then encode item 3's bytes and return `Some`, closing the
   inquiry. The agent takes an answer or refusal command only when its job, generation and request
   id name the open inquiry, and drops any other as stale. The host moves the inquiry from
   open to timed out (on `HmiTimedOut` or its own fallback clock, item 6) under the same lock that
   takes commands, so exactly one of the two wins: an answer taken first is returned even if the
   poll comes later, and one that arrives after the timeout is dropped as late (the server keeps
   it as audit data) and the next poll returns the timed-out answer. The agent
   checks only the shape (`value` is text of at most 16 KiB minus one, empty when `ok` is false)
   when the command arrives, before any poll can return it; a command of another shape is refused
   to the server and the inquiry stays open, so the host never hands the VM a malformed answer;
   the server, which has the form or template, validates the operator's input against it
   (choices, criteria, required items) before it answers, and for a record stores the record
   itself (design 4.3.1); only the judgment reaches the agent. A new host instance, which every
   restart creates, opens a new `request_id` for the same inquiry number: the server supersedes
   the job's open inquiry, and a late answer to a superseded id is audit data only. Because a
   restart may repeat the primitive (it is `Safe`, ADR-335) after the server stored a record but
   before the agent's checkpoint after it, a record must never count twice. Within one run the
   inquiry number is unique, since `steps` only grows. Across runs the agent tells the server where
   each run starts: before a run's first instruction (a first run or any restart) it sends
   `EventBody::HmiRunStart { job_id, generation, from_inquiry }` with the inquiry number of the
   state it starts from, 0 for a run from the start (ADR-335). Everything the abandoned run did at
   or after that number is what the new run repeats, so the server supersedes the job's records
   whose inquiry number is at or above `from_inquiry`, which stay as audit data only, and keeps
   those below it, which the resumed state already holds. The job's active records are therefore
   exactly those of the steps the final run did not repeat plus those it made, with no duplicate
   whatever branch a repeated read takes. The answer itself is never reused. A server
   refusal (an id the server does not have, params the form cannot take) is a command of its own,
   `HmiRefused { job_id, generation, request_id, reason }`: the next poll closes the inquiry and
   returns `HostError::HmiRefused`, a resumable host error and not an answer, so a resume at the
   instruction opens a new inquiry. Whenever an inquiry ends without the operator's answer, the
   host sends `EventBody::HmiWithdrawn { job_id, generation, request_id, timed_out }`, so the server takes
   the form off the screen: when it closes the inquiry with the timed-out answer (item 6), and when
   the run ends while the inquiry is still open (a cancel taken at the waiting instruction, the
   HMI timeout's included, or any other end of the run), before the job's final state is
   reported. The stream and Web API schemas change accordingly: `hmiRequest`
   carries the generation, `kind`, `id` and `params`; `hmiResponse` and the `POST
   .../hmi/{requestId}` body are `{ ok: boolean, value: string }`; the acceptance, the timeout, the refusal, the withdrawal and the run start
   are messages of their own.
   Only the server can answer, since design 5.7.1 lets the browser's local connection to an
   offline agent start pre-fetched jobs and read their progress, nothing else. A job whose program
   holds an `HmiRequest`, `HmiRequestFromStack` or `RecordInput` instruction therefore cannot be
   started offline, like one that needs a seed-key computation or OEM authentication: the agent
   refuses the offline start, and the web UI gives the operator form as the reason in advance
   (design 5.7). A job that loses the server while it runs keeps waiting, as design 8.2.5's table
   says, until the server returns or the HMI timeout ends the wait (item 6). A local path that
   lets the on-site operator answer offline is a follow-up of its own.
6. **The runner polls an HMI wait as it polls a timer.** `Waiting(WaitingOn::Hmi)` reports
   `JobState::AwaitingHmi`, sleeps `wait_poll` and steps again, keeping the keep-alive and the
   cancel check; `JobError::Unanswerable` remains for `SeedKey` only. When the HMI timeout of
   design 5.6 expires, the runner raises the job's cancel flag and the host closes the inquiry with
   the timed-out answer (status 2) on its next poll, telling the server the inquiry is withdrawn
   (item 5).
   Outside every uninterruptible and recovery-required section the waiting instruction is an
   interruptible point, so the runner takes the cancel before that poll and the job ends there;
   the inquiry is still open, so the host withdraws it as the run ends (item 5).
   Inside such a section ADR-330 holds the cancel, so the instruction completes with the timed-out
   answer, the procedure runs on to the section's end (it may test `timed_out` and `diag.fail`),
   and the held cancel is taken at the next interruptible point, as design 5.6 asks. Without that
   answer a wait in such a section could never end. A run whose HMI timeout fired never ends as
   Completed: when the procedure reaches its end while that cancel is still held, which ADR-330
   item 2 would let finish, the job ends as cancelled by the HMI timeout instead, as it would have
   at an interruptible point. Every instruction ran, and the job's outcome still says the operator
   never answered. The server owns the deadline, so no device clock
   is compared with a server time. When the server accepts an `HmiRequest` and shows the form it
   starts the job's wait (its own clock, the job's HMI timeout) and answers `HmiAccepted { job_id,
   generation, request_id }`; when the wait expires with no answer it closes the form, takes no
   later answer, and sends `HmiTimedOut { job_id, generation, request_id }`. The wait belongs to
   the job, not to one request id: a request that supersedes an accepted, unanswered inquiry of
   the same job (a restart's new request id, or a resend after a lost acceptance) continues it,
   and one that arrives after it expired is answered with `HmiTimedOut` again. The host resends
   its request until it has an acceptance or a timeout. While the server cannot be reached the
   host also times the wait on the agent's monotonic clock from its first send, so an unreachable
   server still ends it. Either way the runner commits `Record::HmiTimedOut` to the journal
   before it raises the cancel and before the host returns the timed-out answer. The record is a
   fact about the job that nothing clears, a later answered form included: a run that resumes with
   a `HmiTimedOut` record anywhere in the journal starts with the cancel raised and the timeout as
   its reason, whether it resumes from a checkpoint or starts again from the first instruction,
   so the record needs no key. A handover (ADR-229) needs no message from the failed device: the server marks the job
   timed out when its own wait expired, or when the failed device's withdrawal reported a timeout
   (`timed_out` set), and puts the mark in the instruction it signs for the receiving device,
   whose run then starts with the cancel raised. The timeout of design 5.6 is the server's wait;
   the agent's own clock stands in for it only while the agent cannot hear the server, and binds
   the device that fired it, through its journal, but not the job after a handover. A timeout only
   the failed device's local clock saw, and could not report, therefore leaves the server's wait
   to decide, as it does for every inquiry the server accepted: an operator who answers within
   that wait on the receiving device answered within the job's timeout; an inquiry it never accepted was never shown, and the receiving
   device asks it with a fresh wait. A procedure that ends by `diag.fail` or an error while the cancel is held ends
   that way, as ADR-330 item 2 says. Where the timeout's value comes from (the job
   instruction or the agent's settings) is a separate decision.

Alternative:
- **Keying the server-side inquiry by (job, inquiry number) across restarts.** A replay from the
  start (ADR-335) reaching the same step count would receive a previous run's answer without the
  operator; a request id per host instance costs at most one repeated question after a restart.
