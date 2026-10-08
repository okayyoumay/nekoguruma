# ADR-251: Copilot Re-Review Only After Fixes That Need It

**Date:** 2026-10-08
**Status:** Accepted
**Affects:** `.claude/skills/pr-review-loop/SKILL.md` (Steps 0, 4 and 5), `.github/copilot-instructions.md` ("Writing findings"), `CLAUDE.md` ("Pull requests")

## Context

The `pr-review-loop` skill re-requested a Copilot code review after every round that pushed a
fix, until a round came back clean. Each Copilot review reads the whole pull request diff again
and spends the owner's GitHub AI credits; since GitHub moved the default review effort to
Balanced, each review costs several times what a Lite review does. The reviews recorded on
recent pull requests show where the rounds go:

- A pull request that only re-triaged backlog items in `work/` took eight reviews. From the
  second review on, each one raised a single new Low- or Medium-rated finding, listed under
  "Previously missed" on lines the pull request had not changed since the previous review.
- A pull request that added a GitHub Actions workflow took seven reviews, and most of them found
  real problems in the workflow's trust boundary.

The second kind of loop is the review doing its job. The first kind repeats a full-price review
to confirm a wording fix in prose that CI and the backlog check script already validate.

GitHub's review request through the MCP server cannot choose a lower effort for a re-review, so
the number of re-reviews is the lever available to the session.

## Decision

1. **Re-request only when the fix needs another look.** After a round that pushed a fix, the
   loop re-requests a Copilot review only when the round fixed a finding Copilot rated above Low
   (or whose rating cannot be read), the fix went through `design-advisor`, the round newly
   declined a finding (which gets the same one follow-up review it gets in a round that changes
   nothing), the push changes any file that is not Markdown, or the round merged `main` and
   resolved a conflict. Otherwise the
   loop ends after the round's replies and goes to close-out. Defining the exempt set as
   Markdown only, rather than listing executable file types, keeps a newly added kind of
   executable or data file on the re-review side by default.
2. **Unreviewed fixes stay visible.** Such a round's fix still passes every local check of the
   skill's Step 3 (including `edge-case-hunter` when its gate applies). The pull request
   description and the hand-over message to the maintainer name the findings fixed after the
   last Copilot review, so the maintainer's review covers them.
3. **Ask Copilot to converge.** `.github/copilot-instructions.md` asks Copilot to report
   everything in its first review. On a re-review it reports findings in changed code, and on
   unchanged code it reports P0 and P1 findings (whatever severity it gives them) and anything
   it rates Medium or higher. Only Low-rated findings on unchanged code that are neither P0 nor
   P1 are reduced to a count, so the convergence never hides a more serious finding.

4. **No Copilot review for `work/`-only pull requests.** A pull request whose every changed
   file is under `work/` gets no Copilot review at all. Those files are temporary task lists
   whose format `scripts/check-backlog.sh` and `scripts/check-work-refs.sh` already check in
   CI, and the maintainer reviews their content. Once a push makes the pull request change any
   file outside `work/`, the normal loop starts.

The severity comes from Copilot's own rating in the review summary, not from Claude's judgment,
so the session cannot talk itself out of a re-review for a finding Copilot considered important.

## Consequences

- Backlog re-triage pull requests like the one above, which change only `work/`, now cost no
  Copilot review (Decision item 4). Copilot's findings on them (duplicated or bundled items,
  stale counts in the description) are left to the maintainer's review.
- Other loops end once the remaining findings are Low-rated Markdown
  fixes. Medium-rated findings still trigger a re-review, so the saving on such loops depends on
  the convergence instruction (Decision item 3) as well; its effect on Copilot is to be judged
  from the next pull requests.
- A Low-rated Markdown fix can merge without Copilot having seen it. The maintainer's review is the
  remaining check, which is why Decision item 2 names those fixes.
- Copilot may still report low-severity findings on unchanged code despite Decision item 3; the
  loop then fixes them and, under item 1, does not re-request for them alone.
- If GitHub's MCP review request gains an effort parameter, re-reviews at Lite effort become
  another option to weigh against this rule.
