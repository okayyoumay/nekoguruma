# ADR-246: The CodeQL Hand-Off Carries High and Critical Alert Details Privately

**Date:** 2026-10-07
**Status:** Accepted
**Affects:** `.github/workflows/codeql-alert-handoff.yml`, `.claude/skills/codeql-alerts/SKILL.md`

## Context

ADR-243 had the `codeql-alerts` loop skip high and critical CodeQL alerts (Decision item 3),
since a public PR would disclose the weakness before the fix ships, and sent them to the
routine as alert numbers only (item 4), so they could not crowd the alerts a run can fix out
of the trimmed payload. The receiving session then had nothing to triage them with: it could
not tell a false positive from a real weakness, nor say where a real one is. And a real alert
that no outside input can reach, such as one that needs a vendor library to break its own
contract, waited for a private fix although a public PR would disclose nothing usable.

The details are not public on that path. The workflow sends them only in the routine's fire
request; they never reach the workflow log, an artifact or the repository, which is public.

## Decision

1. **Same details for every alert.** The payload's `private` list carries the same fields as
   `alerts` (number, rule, severities, place, message, creation time), plus `private_total`,
   the count of all open high and critical alerts.
2. **Separate shares of the payload.** The high and critical list gets a byte budget of its
   own and the other list the rest of the payload budget, each filled whole alert by whole
   alert, so neither list crowds out the other. High and critical alerts can stay open until
   the maintainer fixes them, so a fixed first part would starve the rest: when they do not
   all fit, their list is split into consecutive pages that each fit the budget,
   and each run sends one page picked at random, so over successive runs every one is sent.
   The workflow's run number is not used to pick the page: runs that exit early also advance
   it, so some pages could be skipped every time, and a stored cursor would need state the
   workflow cannot keep reliably.
3. **Public fix only when nothing exploitable is disclosed.** The run verifies high and
   critical alerts like any other. A false positive is reported with a recommended dismissal
   reason. A real one that input from outside the trust boundary cannot reach is fixed in an
   ordinary public PR, which says why it is not reachable: the PR discloses nothing an attacker
   could use. A real one that such input can reach, or may reach, gets no PR, issue, branch or
   commit, and its details go into no repository file or GitHub comment; it is reported inside
   the project with its rule, place and reachable path, and the maintainer decides how it is
   fixed. The report also says how many high and critical alerts the payload left out.

## Alternatives considered

- **Keep numbers only.** Rejected: the maintainer and the session have to look each alert up
  by hand before any triage.
- **Report every real high or critical alert privately, never fix it in a public PR.**
  Rejected: for an alert outside input cannot reach, the PR discloses nothing an attacker
  could use, while the fix waits on the maintainer.
- **Encrypted workflow artifact.** Rejected for the same reasons as in ADR-243: an extra key to
  manage, and the fire request is already non-public.

## Consequences

- High and critical alert details live in the routine session's conversation and in the
  project's thread, both visible only to project members.
- A trimmed `private` list leaves some alerts unverified in a run; the report names how many,
  and later runs send the next pages.
- Whether outside input can reach the flagged path is the session's judgment, made against
  the trust boundary in `docs/system-architecture.md`. When it is not clear, the alert is
  treated as reachable and reported privately; a wrong "unreachable" call would disclose a
  weakness, so the doubt goes to the private side.
