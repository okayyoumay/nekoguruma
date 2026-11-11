# ADR-455: The Agent's Communication Log Goes to the Server Only by Explicit User Action

**Date:** 2026-11-11
**Status:** Accepted
**Affects:** design 8.11, 16.2; `agent` communication log, the server's storage of a job's result (M3)

## Context

The agent keeps a per-job communication log in local storage (design 8.11, ADR-346, ADR-347). It holds VINs and diagnostic content, which design 16.2 treats as troubleshooting-log data: sending it is limited to explicit user action, with the scope shown and a retention period defined, and only data with identifiers anonymized may be sent automatically. Its upload and retention were left open.

## Decision

1. The log is sent to the server only by explicit user action. An operator selects the job or jobs and is shown the scope (the period and the target jobs) before the agent sends anything.
2. The agent then uploads that job's log over the data channel (design 5.2) as a must-deliver transfer. It deletes nothing locally before the server acknowledges the upload, and otherwise keeps its local copy under its own rules.
3. The server keeps the uploaded log with the job's result for a defined retention period: the job result's retention, access and VIN-deletion handling (design 4.3, ADR-237), not a record kind of its own. A period of its own is left to the cloud deployment (M8).
4. Automatic sending is limited to data with identifiers anonymized. That is not done in M3 and is a later item.
5. Rejected: uploading the log automatically with the job's result once the job's last run has ended. It would send VINs and diagnostic content without a user's action, which design 16.2 does not allow.

## Consequences

- In M3 the operator action can be a Web API operation; viewing and export of a stored log stay with the browser work (M4).
- The upload needs the data channel, the object storage and the sequence-numbered outbox that the job control and server work bring.
- A job whose log nobody uploads leaves it on the agent only; the server holds no log for it.
