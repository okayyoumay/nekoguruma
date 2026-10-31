# ADR-398: A Refused Connect-Time Filter Fails the J2534 Connect; the Log Header Names the Job Kind; `ngr-agent identify`

**Date:** 2026-10-31
**Status:** Accepted
**Affects:** `j2534-0404-service` (`rpc_link`: the connect's install of the UniqueRespIdTable
filters and the teardown shared with `DisconnectComLogicalLink`; `error`; `events`: the hard
error clears the counts), `j2534-0404-mock`, `agent` (`comm_log` `RunFacts` and header, `runner`, `identification`, `main.rs`),
`crates/agent/docs/ngr-agent.md`, `docs/j2534-0404-architecture.md`, `docs/rpc-api-guide.md`,
`crates/j2534-0404-service/docs/implementation-notes.md`, design 8.8.2 and 8.11; ADR-039,
ADR-068, ADR-080, ADR-105, ADR-129, ADR-162, ADR-294, ADR-327 item 2, ADR-393, ADR-394,
ADR-395, ADR-396 items 7 and 8, ADR-397

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
   install. The filters the table took so far are given back (item 8), the link is taken
   offline by the teardown `DisconnectComLogicalLink` runs (item 10, which also rolls back the
   shared-channel reference, or the channel this connect opened), and the connect returns the
   status item 9 names. A failure while taking the link offline, such as a filter the adapter
   will not stop, is logged and does not replace the connect's status. The same applies to a
   link joining a channel another link holds: the sibling is untouched.
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


8. **A channel's point-to-point filters are counted.** `SharedChannel::p2p_filters` holds one
   slot per distinct filter on the channel, identified by its type, the protocol id of its
   messages, its pattern address and its flow-control address, with the number of links using
   it; a link's `unique_resp_filter_ids` lists the filters it uses. An entry whose filter the
   channel already holds, from a sibling's table or a repeat in the link's own, is counted and
   not installed again, since an adapter refuses a repeated pattern or flow-control
   identifier (a functional presence link and a physical link to one ECU both name that ECU's
   pair). A filter is stopped when its last user leaves: `DisconnectComLogicalLink`,
   `DestroyComLogicalLink`, the teardown of a failed connect, the promotion of a table
   (the old table's uses are given back before the new one's are taken) and the reinstall after
   `CLEAR_MSG_FILTERS` (which counts afresh, since the clear removed every filter). The counts
   are part of the channel's entry, so closing the channel ends them, and a channel hard error
   clears them together with the links' lists (the filters died with the channel). They change
   under the `shared_channels` guard, which the connect now holds from the publication of the
   link through the install (ADR-080 order: `shared_channels`, `logical_links`, `api`); the CAN
   mode is read before the guard is taken (the AB-BA rule in
   `install_point_to_point_fc_filters_core`).
9. **Three kinds of connect failure at the install.** The filter statuses `ERR_NOT_UNIQUE`,
   `ERR_EXCEEDED_LIMIT`, `ERR_INVALID_MSG` and `ERR_MSG_PROTOCOL_ID` are the adapter's refusal
   of the table: `Code::Internal` with the `PDUError` of the native status (invalid parameters
   for the first, third and fourth, a resource error for the second). `ERR_DEVICE_NOT_CONNECTED`
   is a lost VCI (ADR-294): `FAILED_PRECONDITION` with `PDU_ERR_EVT_LOST_COMM_TO_VCI` attached,
   the form the worker answers a call on a link the loss took offline (ADR-105), which the
   agent reads as `names_lost_vci` and reopens on (ADR-393, ADR-394, ADR-396), not as a table
   to halve. Any other error fails the connect as `Code::Unknown` with `PDU_ERR_FCT_FAILED`:
   the agent's `refused` never reads `UNKNOWN` as the worker's answer, so such an error is not
   a refusal and the table is not halved, with no change to the agent. The three are told apart
   in `error::map_connect_filter_failure`. In `auto` CAN channel mode the probe of ADR-047 runs
   before this install, outside the `shared_channels` guard, so the filters installed are the
   resolved mode's: an adapter with a companion channel is not refused the UUDT workaround's
   second flow-control filter that a single-channel table would need.
10. **One offline teardown (ADR-397).** `DisconnectComLogicalLink` and the rollback of a
    refused connect run one function, `take_link_offline`, under the `shared_channels` guard
    the caller holds: client filters, point-to-point filters (item 8), repeat messages, the
    TP2.0 broadcast periodic message, J1939 claims and TP2.0 connections, the link's COPs
    (`cancel_link_cops`), the Offline status, the occupancy epoch of a rolled-back join, the
    channel reference (closing the channel when the link was its last user), the state the
    link carries while online (communication started, a stop pending, tester present, the open
    tester-present discards, the lock mask) and the dual-channel UUDT companion. The rollback
    passes the connection it set up: the teardown acts only if the link still holds that
    channel key (matched on the key alone, since a channel hard error clears `channel_id` but
    keeps the key), and stops filters only on the channel whose id the link was connected on.
    A link that never was online also gets an Offline status: `take_link_offline` is the one
    teardown for both callers, so a refused connect leaves the link in the same Offline state
    that a disconnect leaves, whatever state it had reached. This rests on the worker's own state handling, not on a clause of
    ISO 22900-2.

## Consequences

- A receive table the adapter refuses is a refusal the agent sees, so ADR-396's halving works
  against the J2534 worker; the halving is tested against `sim-vci` (a non-unique response
  identifier, and the filter limit).
- A client that used a UniqueRespIdTable the adapter cannot fully hold used to get a link with
  fewer filters; it now gets a failed connect. A table that exceeded the adapter's filter limit
  at connect was already a loud, if late, condition on such hardware.
- The program runner's one-pair link, if refused, now fails at open with the worker's status
  instead of coming up without a response path.
- The promote path still skips a refused entry; a backlog item tracks it. It does give back the
  old table's filters and take the new table's through the counts (item 8).
- A link that leaves, or reconnects, beside a sibling no longer leaves its filters on the
  channel, so the reconnect that the strict install would have refused as not unique succeeds.
- A device lost during the install is the lost VCI the agent reopens on; an error that is not
  the table's is a failed connect the agent does not halve on.
- ADR-039 and ADR-068 describe filter installation as best-effort; for the connect that is
  replaced by item 1 (their annotations say so). ADR-396 item 8 is realized.
- The plan's signature is still unverified (ADR-395 item 2); a backlog item tracks it.
