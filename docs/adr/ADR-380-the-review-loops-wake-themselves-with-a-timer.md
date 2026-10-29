# ADR-380: The Review Loops Wake Themselves with a Timer

**Date:** 2026-10-29
**Status:** Accepted
**Affects:** `.claude/skills/pr-review-loop/SKILL.md` (Step 0, Codex and Copilot waits; Step 5's CI wait), `.claude/skills/backlog-loop/SKILL.md` (step 6); ADR-360 Decision item 2

## Context

`pr-review-loop` ends its turn after each review request and after each push, and `backlog-loop`
ends its turn while a handed-over pull request waits for the maintainer's merge. The session
wakes again on a GitHub event: a submitted review, a CI result, a merge. As a safety net the loop
scheduled a one-shot check-in with `send_later` about 15 minutes after each review request, and
ADR-360 counted a Codex request with no result after 30 minutes as stalled.

Measured over 48 consecutive `backlog-loop` pull requests, about a fifth of the time Claude spent
on them was spent waiting with nothing running. Two cases were traced through the session's
transcript:

- A Codex review stalled (the known service-side behaviour ADR-360 works around). The session's
  `send_later` call failed because the tool was not offered in that session, and no other timer
  existed. Nothing woke the session for more than two hours, until the maintainer posted a review
  request by hand.
- The event for the head commit's finished CI run was never delivered; only the event of a
  superseded, cancelled run arrived. The session waited idle for over an hour, until the
  maintainer asked for a status.

So GitHub events cannot be relied on to arrive, and `send_later` cannot be relied on to exist. A
session can always start a shell command in the background, and the session is re-invoked when
that command exits. Answered Codex requests in the same sample took a median of under two
minutes, and 95% took under seven.

## Decision

1. **A background `sleep` is the timer.** Whenever a loop ends a turn waiting for something on
   GitHub, it first starts `sleep <seconds>` as a background shell command; its exit wakes the
   session. The loops no longer use `send_later` or other scheduled triggers for these waits.
2. **One timer per wait, with fixed lengths:**
   - after a review request (Codex or Copilot): 15 minutes;
   - after a push whose CI the loop waits for before handing over: 10 minutes, then 5 minutes
     while any check run of the head is still queued or in progress;
   - while a handed-over pull request waits for the maintainer's merge: 30 minutes, repeated.
3. **Every wake starts from GitHub's state.** A wake, from an event or a timer, first reads the
   pull request's current state (head commit, the Codex summary state, reviews after the latest
   request, the head's check runs, open, merged or closed) and does only what that state still
   needs. A wake that finds nothing new starts the next timer of the same wait and ends the turn,
   so an event that arrives late, or a timer that fires after the result, changes nothing.
4. **A Codex request counts as stalled after 15 minutes** (ADR-360 item 2 said 30): when the
   timer after the request finds no result, also while the summary still shows the review running.
   ADR-360's one retry per head, its quiet retry and the switch to Copilot after a failed retry
   are unchanged; the retry gets its own 15-minute timer.

## Consequences

- A stalled review or a lost event costs at most one timer length instead of an open-ended wait,
  and the loop no longer depends on the maintainer to notice it.
- Requests that would have finished between 15 and 30 minutes are retried. In the sample that is
  about 2% of answered requests; each spends Codex usage allowance once more, and one retry per
  head still bounds it.
- When Codex stalls on every request, the switch to Copilot comes after about 30 minutes instead
  of an hour.
- A background `sleep` keeps the session's container busy for its length. The longest timer is
  the 30-minute merge wait; a container reclaimed anyway loses the timer, as it would lose any
  other in-session state, and a new session resumes from the pull request (ADR-230).
