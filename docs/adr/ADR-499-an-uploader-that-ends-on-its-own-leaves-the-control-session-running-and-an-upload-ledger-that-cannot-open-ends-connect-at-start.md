# ADR-499: An Uploader That Ends on Its Own Leaves the Control Session Running, and an Upload Ledger That Cannot Open Ends `connect` at Start

**Date:** 2026-11-21
**Status:** Accepted
**Affects:** design 5.2; `crates/agent` (`connect`, `uploader`, `upload_ledger`), `crates/agent/docs/ngr-agent.md`, `crates/agent/docs/operational-log.md`; details ADR-493 item 10, relies on ADR-495

## Context

ADR-493 item 10 makes the uploader the second task of `ngr-agent connect`, beside the control
session, with its own token exchange: neither channel waits on the other. It does not say what
the agent does when one of the two ends while the other still runs, or when the upload ledger
cannot be opened at all. ADR-495 gives the uploader one way to end on its own: a poisoned ledger
that, opened again, does not read back (`Ended::LedgerUnusable`, ADR-495's loop rows). A panic of
the task ends it too, with no reason. Restarting the agent does not repair a ledger that does not
read back; an operator must remove or repair it, as for the other state files. The maintainer
decided both cases on 2026-11-21.

## Decision

1. **An uploader that ends on its own leaves the control session running.** `connect` logs it
   once at `error` ("the uploader ended; nothing is uploaded until the agent restarts, and the
   control channel session goes on"). The session goes on, and `connect`'s exit status stays the
   session's. Nothing is uploaded until the agent restarts. The uploader is not restarted
   in-process. Exiting `connect` instead was rejected: it would stop job delivery too, and a
   restart by the service manager does not repair the ledger. A restart of the uploader in-process
   was rejected: against a ledger that does not read back it repeats the failure.
2. **An upload ledger that cannot be opened at start ends `connect`**, as the other state files
   do (the journal, the received-jobs ledger): a ledger that does not read back is exit 1 (the
   operator repairs it), one in use or unreadable is exit 3 (retried). Starting the control session
   without an uploader was rejected: the agent would run with uploads stopped and nothing at start
   to show it.
3. **A stop ends both tasks**, and `connect` waits for the uploader at most 10 seconds. A session
   that ends on its own error is followed by the same stop and wait, so that the process's short
   shutdown does not cut a ledger rewrite.

## Consequences

- An agent whose ledger stopped reading back mid-run keeps answering commands but uploads
  nothing; the operator sees it only in the agent's log until reporting it to the server is
  built.
- A corrupt ledger found at start keeps the agent from connecting at all, so job delivery waits
  for the repair.
