# ADR-246: The CodeQL Hand-Off Carries High and Critical Alert Details Privately

**Date:** 2026-10-07
**Status:** Accepted
**Affects:** `.github/workflows/codeql-alert-handoff.yml`, `.claude/skills/codeql-alerts/SKILL.md`

## Context

ADR-243 Decision item 4 sent high and critical CodeQL alerts to the `codeql-alerts` routine as
alert numbers only, so they could not crowd the alerts a run can fix out of the trimmed
payload. The receiving session then had nothing to triage them with: it could not tell a false
positive from a real weakness, nor say where a real one is.

The details are not public on that path. The workflow sends them only in the routine's fire
request; they never reach the workflow log, an artifact or the repository, which is public.

## Decision

1. **Same details for every alert.** The payload's `private` list carries the same fields as
   `alerts` (number, rule, severities, place, message, creation time), plus `private_total`,
   the count of all open high and critical alerts.
2. **Separate shares of the payload.** Each list is trimmed to half the payload budget before
   the whole payload is trimmed, so neither list crowds out the other.
3. **Verified, reported inside the project only.** The run verifies high and critical alerts
   like any other. It never opens a PR, issue, branch or commit for them and never writes their
   details to a repository file or a GitHub comment. A false positive is reported with a
   recommended dismissal reason; a real one is reported in the project with its rule, place and
   reachable path, and the maintainer decides how it is fixed. The report also says how many
   high and critical alerts the payload left out.

## Alternatives considered

- **Keep numbers only.** Rejected: the maintainer and the session have to look each alert up
  by hand before any triage.
- **Encrypted workflow artifact.** Rejected for the same reasons as in ADR-243: an extra key to
  manage, and the fire request is already non-public.

## Consequences

- High and critical alert details live in the routine session's conversation and in the
  project's thread, both visible only to project members.
- A trimmed `private` list leaves some alerts unverified in a run; the report names how many,
  and the next run picks them up.
