# ADR-508: An Operator's Request Row Is What Lets a Communication Log Be Stored, and the Agent Is Told by an Unsigned Control Message

**Date:** 2026-11-25
**Status:** Accepted
**Affects:** design 4.3, 8.11, 13.4, 14.3; `crates/server` (`uploads.rs` begin, a migration for `comm_log_requests`, the hub's delivery, the Web API), `crates/agent` (the handling of the command and the log's path), `db/README.md`, `api/openapi.yaml`, `docs/glossary.md`; details ADR-455; amends ADR-493's judgement of begin and two of its Consequences

## Context

ADR-455 fixes the rule: a job's communication log reaches the server only by an operator's explicit action, with the scope shown, and the agent then sends it over the data channel. ADR-493 built the channel and left open how an operator's action reaches the agent, and how a source that later runs append to is handled (its Consequences). Three facts follow from the code as it stands:

- Begin (ADR-493 item 3) accepts any `commLog` for a job of the calling agent and generation, so an agent could store a log nobody asked for, which design 16.2 does not allow.
- Nothing tells the agent that an operator asked: the agent's uploader sends what its ledger holds, and the ledger is filled by a debug-only command.
- Nothing deletes or ages a job's stored objects, so a log's retention has no mechanism to attach to.

## Decision

After step 1 only item 3's refusal, the table and item 6's rule that a stored log is not replaced exist. Items 1 (the write of a request), 2, 4 and 5 are decided here and built in steps 2 to 4 (item 7).

1. **A request row per job and generation.** An operator's request is a row of `comm_log_requests` keyed by tenant, job and ownership generation, with the agent the job belongs to, the operator who asked, a state (`requested`, `accepted`, `refused` or `expired`) and, for `refused`, the agent's reason (`unknownJob`, `noLog`, `busy`, `tooLarge`). "Stored" is not a state: a stored log is the job's `job_objects` row of kind `commLog` that is `finished`. The row is written by a Web API operation that is bound to a scope the operator was shown: a first call lists the jobs, the period and a digest of what it shows (1 to 32 jobs), and a second call names the jobs and the digest, so a request is made only for a scope that is still the one displayed. The call is one transaction, all or nothing, and writes one audit row per newly requested job. A request for a job and generation whose row is `refused` or `expired` renews it by an update in place: the state returns to `requested`, `refusal`, `sent_at` and `answered_at` are cleared, and `requested_by` and `requested_at` become the new request's; a row in state `requested` or `accepted` is left as it is. The table keeps one row per job and generation, so a renewal cannot add a second. The agent's job is the row's: its `agent_id` is tied to the job's by a composite foreign key.
2. **Who may ask.** An operator with the `worker` permission, for a job the operator created, whose agent is not revoked, whose job has been delivered and has ended.
3. **Begin refuses a log nobody asked for.** For kind `commLog` only, begin requires that a live `job_objects` row exists, or that a request row exists for the caller's agent, the job and the declared generation in state `requested`, `accepted` or `expired`. Otherwise it answers 404, the answer for an unknown job, and begins nothing in the store. `accepted` admits begin because the agent may begin before the server has applied its answer. `expired` admits it because expiry only stops the server's delivery: an agent that queued the log just before the expiry, or whose answer was lost, must still be able to send it. A `refused` row admits no begin: the agent said it has no log to send (or cannot send it now), so a begin is not what it answered, and a later request renews the row (item 1). A repeated begin of an upload that exists is answered from that row whatever the request's state has become. Kind `result` is not affected.
4. **The agent is told by a control message, sent at least once.** The server sends `logUploadCommand` (job, generation) on the control channel to the agent's connected session, again after a lease of 90 seconds until the agent answers `logUploadAnswer` (`queued`, `unknownJob`, `noLog`, `busy`, `tooLarge`) or the request is 24 hours old, when its state becomes `expired`, which stops the delivery and nothing else. The command is not signed: it only asks the agent to send what the agent holds for a job it has already accepted, to the server that issued the job, over the data channel where the server's own checks apply; a forged command can make the agent send a log early, not send it elsewhere or send other bytes. The standard profile revisits that (below).
5. **The agent acts only for a job it holds.** It answers `unknownJob` unless its received-jobs ledger holds the job at that generation. The log is read from its own path (`{job}.g{generation}.commlog.jsonl` in the journal directory of its mode), never from a path the server names, as the bytes up to the last line end, under the writer lock of the log, tried once (`busy` if another writer holds it). A missing or empty file is `noLog`, a file over the object limit is `tooLarge`, an entry already pending answers `queued` without hashing again (that check comes before the writer lock is tried, so a redelivered command for a queued log never answers `busy`), and the answer follows the durable enqueue.
6. **The log is the job's `commLog` object, beside its `result`.** It has no retention period and no column for one: whatever later removes a job's result removes both, with the request rows. One log is stored per job and generation, and a stored log is not replaced.
7. **Built in five steps.** (1) This decision, the table and begin's refusal. (2) The two control messages and the agent's handling. (3) The server's delivery and its application of the answers. (4) The Web API, whose second call renews refused and expired rows as item 1 says. (5) An end-to-end test with the real agent, server and object store. After step 1 the table exists, nothing writes it outside tests, and begin refuses every log of the upload route without a row.

Rejected:

- **A signed command.** A signature protects nothing the data channel's checks do not, in M3's local profile; it would add a purpose and a payload form for a message that only asks.
- **The agent polling for requests.** The control channel already reaches a connected agent, and a poll would add a route, a timer and a load on the server for an event that is rare.
- **A stored proposal the agent fetches.** It needs a route of its own and a state to expire, for the same rare event.
- **A retention column on the job's objects now.** The period depends on the tenant and record kind of design 4.3, which are not built; a column written now would be a second place for the rule.
- **Replacing a stored log.** A later run appends to the log, so the stored object would silently differ from the one an operator was shown, and a replacement would need the object's old key kept or removed in step with the row.

## Consequences

- One stored log per job and generation. A run that appends after the log was stored is not sent again.
- Nothing deletes stored logs or request rows yet. This is acceptable in the local profile only, and a pass over a job's objects and request rows, with VIN deletion, is needed before any deployment that is not local.
- The runner's intake must write every job's log at the path of item 5 and enqueue the result for the uploader, or a requested log answers `noLog`.
- On the standard profile the command's authentication is revisited with the profile where the server's answer proof is not required (ADR-504 item 6).
- ADR-493 item 3's judgement of begin gains item 3 above, and its Consequences on the source that changes and on how an operator's upload reaches the agent are answered by items 4 to 6.
