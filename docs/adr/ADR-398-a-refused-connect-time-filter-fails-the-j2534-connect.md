# ADR-398: A Refused Connect-Time Filter Fails the J2534 Connect; the Log Header Names the Job Kind; `ngr-agent identify`

**Date:** 2026-10-31
**Status:** Accepted
**Affects:** `j2534-0404-service` (`rpc_link`: the connect's install of the UniqueRespIdTable
filters), `agent` (`comm_log` `RunFacts` and header, `runner`, `identification`, `main.rs`),
`crates/agent/docs/ngr-agent.md`, `docs/j2534-0404-architecture.md`, `docs/rpc-api-guide.md`,
`crates/j2534-0404-service/docs/implementation-notes.md`, design 8.8.2 and 8.11; ADR-039,
ADR-068, ADR-129, ADR-162, ADR-327 item 2, ADR-395, ADR-396 items 7 and 8, ADR-397

## Context

ADR-396 made the identification runner halve a receive table the worker refuses, but left two
things for the next step. First, the J2534 worker logged and skipped a table entry whose
`PassThruStartMsgFilter` the adapter refused (ADR-039, ADR-068: best-effort filter
installation), so the link came up without that response path and the agent saw no refusal to
halve on. Second, the agent had no command for the identification job, and the communication log's header
could only call a job a procedure; the identification runner also built its log facts with
defaults, so its `vci_connected` lacked the API and the width a procedure's has.

The worker already fails a connect in the neighbouring cases: a staged client filter the
adapter refuses (ADR-129) and a native-mixed identifier collision (ADR-162).

## Decision

1. **The connect fails with the adapter's refusal.** At `ConnectComLogicalLink`, the first
   entry of the connect-time UniqueRespIdTable whose filter the adapter refuses stops the
   install. The filters installed for the table so far are stopped, the link is unpublished,
   the shared-channel reference (or the channel this connect opened) is rolled back through the
   tail the other connect rejections use, and the connect returns the status of the native
   error as `PassThruStartMsgFilter` failures map (`Code::Internal`; the `PDUError` follows the
   native status, such as invalid parameters for a non-unique identifier and a resource error
   for the filter limit). A failure to stop a filter during the rollback is logged and does not
   replace the refusal. The same applies to a link joining a channel another link holds: the
   sibling is untouched.
2. **What stays as it was.** Entries skipped for eligibility (no `CP_CanPhysReqId`, the
   flow-control format bit off, the UUDT sentinel) stay skipped, an empty table installs
   nothing (ADR-048), and the D-PDU worker already passes its refusal through.
3. **The asymmetry with a table set after the connect.** The promotion of a table by
   `CoptUpdateparam` (`promote_unique_resp_id_table`) and the reinstall after
   `CLEAR_MSG_FILTERS` keep logging a refused entry and skipping it. Failing there would have to
   decide what a half-applied reconfiguration of a live link leaves behind, which the connect
   does not face because it can undo everything. The asymmetry is recorded here, not removed.
4. **The log header's job kind is `procedure` or `identification`.** Whether a job writes is
   not a kind: `job_start` carries it, as before. The header of an identification job carries
   the plan's SHA-256 as `plan_sha256` and no document hash.
5. **One facts builder for both runners (ADR-397).** `runner::base_facts` builds the facts
   every run's log opens with (job ID and generation from the log's key, kind, the VCI's
   facts, margins); the procedure runner adds its document, variant, VIN and `writes`, and the
   identification runner adds the plan's hash. `run_identification` takes the VCI's facts, so
   its `vci_connected` names the API and the width. `RunFacts` has no default, so a builder
   states its kind.
6. **One shell for both commands (ADR-397).** `ngr-agent run` and `ngr-agent identify` run
   their job through one function, `run_on_vci`: the log's setup, the guards, the worker's
   launch, its source and slot, the report of the log's path, and the worker's stop with the
   guards' release. A job that does not write takes `JobGuards::take_vci_only`; the identification
   job always does (ADR-396 item 7).
7. **`ngr-agent identify`.** Options: `--plan <file>` (once) and `--document <file>` (any
   number), with `--vci`, `--api`, `--mode`, `--workers`, `--locks` and `--comm-log` as for
   `run`. The report is one line of JSON on stdout. An unsigned plan is accepted until the
   plan's signature is verified. A document the plan does not name is ignored with a warning on
   stderr; the same file twice is one document. The plan and documents are read and checked
   before the log directory, the lock or the worker. Exit status: 0 for a report (an
   interrupted presence section and refused tables included), 1 for an unreadable plan or
   document, a refused plan, a failed job or a worker that may still hold the VCI, 2 for a usage
   error.

## Consequences

- A receive table the adapter refuses is a refusal the agent sees, so ADR-396's halving works
  against the J2534 worker; the halving is tested against `sim-vci` (a non-unique response
  identifier, and the filter limit).
- A client that used a UniqueRespIdTable the adapter cannot fully hold used to get a link with
  fewer filters; it now gets a failed connect. A table that exceeded the adapter's filter limit
  at connect was already a loud, if late, condition on such hardware.
- The program runner's one-pair link, if refused, now fails at open with the worker's status
  instead of coming up without a response path.
- The promote path still skips a refused entry; a backlog item tracks it.
- ADR-039 and ADR-068 describe filter installation as best-effort; for the connect that is
  replaced by item 1 (their annotations say so). ADR-396 item 8 is realized.
- The plan's signature is still unverified (ADR-395 item 2); a backlog item tracks it.
