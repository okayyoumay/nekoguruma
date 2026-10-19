# ADR-312: Codex as the Pull Request Reviewer, with Copilot as Fallback

**Date:** 2026-10-16
**Status:** Accepted (Decision item 4 amended 2026-10-19: comparison done, Codex kept)
**Affects:** `.claude/skills/pr-review-loop/SKILL.md` (Steps 0, 1, 4 and 5), `AGENTS.md`, `CLAUDE.md` ("Pull requests"), `.github/instructions/docs.instructions.md`; ADR-251

## Context

The `pr-review-loop` skill had GitHub Copilot code review as its only automated reviewer. Each
Copilot review spends the owner's Copilot AI credits, and its re-reviews tend to add one minor
finding on unchanged code per round, which ADR-251 works around by limiting re-requests.

Codex, the ChatGPT Codex GitHub app, also reviews pull requests. It is requested with a PR
comment, reports findings as inline comments with P0 to P3 badges, and spends the owner's Codex
usage allowance on their ChatGPT plan instead of Copilot credits or Actions minutes. No published
comparison of the two reviewers' quality was found that would settle which one suits this
repository, so the maintainer chose to switch to Codex for a while and compare its results with
the Copilot reviews already on record.

Codex has two known weak points: its usage allowance can run out (and other users report
refusals for a reached limit while their allowance showed room), and it reads its guidelines
from `AGENTS.md` rather than from the files Copilot reads.

## Decision

1. **Codex reviews; Copilot takes over per pull request.** The loop requests Codex. When Codex
   cannot review a pull request (the app is not set up, a usage limit is reached, or a review
   stalls), the loop switches that pull request to Copilot right away and keeps Copilot for the
   rest of it, telling the maintainer once. It does not wait for a Codex allowance to recover.
   The next pull request starts with Codex again. A pull request never has both reviewers, since
   each reads the whole diff from scratch and a second one adds a round of new findings instead
   of converging. When Copilot cannot review either, the existing `edge-case-hunter` fallback
   applies.
2. **One set of review guidelines.** `AGENTS.md` does not restate the rules: it tells Codex to
   follow `.github/copilot-instructions.md` and the matching files in `.github/instructions/`,
   and maps their terms onto Codex's badges (their P0 and P1 lists are its P0 and P1; "Medium or
   higher" is P2 or higher, "Low" is P3). A rule change therefore still updates the files in
   `.github/` only.
3. **ADR-251 applies to both reviewers.** Its re-request conditions and its exemption for
   backlog-only pull requests hold whichever reviewer the pull request has. "Rated above Low"
   means Copilot Medium, High or Critical, or Codex P0, P1 or P2.
4. **Each pull request records the data for the comparison.** The review summary in the pull
   request description names the reviewer (and why a pull request switched to Copilot), the
   rounds and the number of findings fixed, accepted and declined. After about ten pull requests
   reviewed by Codex the maintainer compares them with the Copilot pull requests before the
   switch and decides whether to keep Codex, return to Copilot or split the work between them.
   **Amended 2026-10-19: the comparison is done and Codex stays the reviewer.** It covered the
   38 pull requests Codex alone reviewed after the switch against the 30 Copilot ones before it.
   On code pull requests the two were close: median 2 rounds each (mean 3.5 for Codex, 3.0 for
   Copilot), about four findings per pull request, and a clean last review on 32 of 34 against 25
   of 26. Codex rated no finding P3, while about a tenth of Copilot's were Low, and it had a few
   more findings declined or accepted (6 of 223, against almost none). Its cost shows on long
   design ADRs, where it kept finding new high-rated cases for 45 to 47 rounds on two pull
   requests. Its usage limit stopped it on four pull requests in three days, and the per-pull
   request fallback to Copilot handled each. Codex stays the reviewer and Copilot the fallback,
   with no change to items 1 to 3; the review summary in item 4 is still recorded, so a later
   comparison can reuse it.

## Consequences

- Reviews no longer spend Copilot AI credits unless a pull request falls back to Copilot; Codex
  reviews run outside GitHub Actions.
- If Codex's usage limit is reached often, many pull requests end up reviewed by Copilot anyway,
  which also shows in the recorded data.
- The loop decides when Codex reviews, so Codex's automatic reviews (on opening a pull request
  for review and on marking a draft ready) stay off in the owner's Codex settings. With them on,
  handing a pull request over would start a review that ADR-251 exempts, or put both reviewers on
  a pull request that fell back to Copilot. If an automatic review arrives anyway, the loop tells
  the maintainer and handles its findings like any other result.
- Whether Codex follows the pointer in `AGENTS.md` as faithfully as it would follow rules written
  there was unverified at first. Its reviews since show that it does: its findings apply rules
  that only the files in `.github/` state, such as a standards citation without its edition and
  clause, or a change to the IR that leaves its golden files out. If that changes, the fallback is
  to copy the rules into `AGENTS.md` and keep both copies in step.
- The re-review convergence wording in `.github/copilot-instructions.md` uses Copilot's severity
  names; Codex relies on the mapping in `AGENTS.md` to read it.
