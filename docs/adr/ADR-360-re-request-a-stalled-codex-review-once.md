# ADR-360: Re-Request a Stalled Codex Review Once

**Date:** 2026-10-26
**Status:** Accepted
**Affects:** `.claude/skills/pr-review-loop/SKILL.md` (Step 1, Codex; Step 5's review summary), `CLAUDE.md` ("Pull requests"); ADR-312 Decision item 1

## Context

ADR-312 Decision item 1 switches a pull request to Copilot as soon as a Codex review stalls. The
loop counted a review as stalled when it had no result 45 minutes after the request, and it
re-requested once only after an explicit error.

Stalls became frequent. In several pull requests in a row, a Codex review showed `running` in its
summary comment and then never finished, with no result and no error. In most of them the
maintainer asked for the review again on the same commit, and that second request finished
normally; in the others the pull request moved to Copilot, which spends the owner's Copilot
credits. The same behaviour is reported publicly against the Codex GitHub review service: a
request is acknowledged and marked running, then stays that way for hours, while a new request on
the same commit often completes in minutes. No cause or fix has been published. The service runs
outside this repository, so the loop can only work around it.

## Decision

1. **One retry per head.** When a Codex request on the current head stalls or ends in an error,
   the loop posts one new Codex request on the same head before switching. Only if that retry
   also stalls or errors does the round count as stalled, and then ADR-312's switch to Copilot
   applies unchanged. An error and a stall share the one retry, so a head never gets more than two
   Codex requests. Each new head gets its own retry.
2. **A shorter wait per request.** A request that is still running, or has no result, 30 minutes
   after it was made counts as stalled (it was 45). A Codex review normally finishes in under ten
   minutes, so 30 still leaves a wide margin, and the worst case before Copilot takes over is
   about an hour instead of 45 minutes with no retry.
3. **The retry is quiet.** The loop tells the maintainer only when the pull request switches to
   Copilot, not when it retries; the review summary in the pull request description records each
   retried request.
4. **Either result counts.** After a retry, a result of either request on the current head is
   the round's result. If both arrive, the second adds only the findings the first did not raise.

## Consequences

- Most stalls end in a Codex review instead of a switch to Copilot, so fewer Copilot credits are
  spent and a pull request keeps one reviewer more often.
- A retry spends Codex usage allowance again, and if the first request finishes after all, Codex
  reviews the same commit twice. One retry per head bounds both.
- When the Codex service stalls on every request, the switch comes about 15 minutes later than
  before.
- If the service is fixed and stalls become rare, the retry costs nothing; if they stay frequent
  even after a retry, the wait or the number of retries can be revisited in a later ADR.
