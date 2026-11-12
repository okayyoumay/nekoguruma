# ADR-349: Pause the Review Loop on Long Specification Pull Requests

**Date:** 2026-10-23
**Status:** Accepted (Decision items 1 and 4 amended by ADR-381: the pause applies to code pull requests too)
**Affects:** `.claude/skills/pr-review-loop/SKILL.md` (Step 4), `CLAUDE.md` ("Pull requests"), `.github/copilot-instructions.md` ("Writing findings"); ADR-251

## Context

A pull request that only writes or amends a specification (an ADR, the design document and the
documents that follow them) can run for dozens of review rounds. Looking at those pull requests
finding by finding showed two things:

- The reviewer reports one to three findings per round, and that number does not fall as the
  rounds go on. The number of rounds is therefore set by how many gaps the specification still
  has.
- Late in a long loop, most findings fall on text that earlier fixes wrote, not on the text first
  sent for review. A fix that closes exactly the reported case adds a rule whose neighbouring
  cases are not settled, so each fix opens the next finding. Some of these late findings were
  P0 safety gaps that a hurried fix had created.

The checks the loop already runs before each request (the sweep for recurring finding kinds,
`edge-case-hunter`, `design-advisor`) did not stop this. They find gaps in the text as it stands,
but they do not stop each fix from adding new text with gaps of its own. The loops that ended
early ended when part of the scope moved to a decision of its own. Whether to cut scope, rewrite
or go on changes what the pull request decides, so it is the maintainer's choice, not the
loop's.

## Decision

1. **Which pull requests.** A specification pull request is one whose changed files, `work/`
   aside, are all Markdown and include an ADR (`docs/adr/ADR-*.md`) or
   `docs/system-architecture.md`.
2. **When the loop pauses.** On a specification pull request, count the review rounds that
   reported findings, whichever reviewer gave them (Codex, or Copilot after a fallback). After
   the eighth such round, and after each later one, the round's fix is still pushed as usual. Before the re-request (also the one Step 3 of the
   skill allows after a round that changed no file), the loop then checks where the findings
   of the last four rounds landed: it runs `git blame` on each finding's line at the commit the
   finding reviewed and counts the line as fix-written when the commit that wrote it came after
   the baseline: the head of the first review request, or after a cut or a rewrite, the head of
   the one review requested after it. A finding with no thread (one only a Copilot review summary
   lists) is placed by the file and line the summary names; one that names no line, and one on
   a deleted line, counts as not fix-written. If more than half of those findings are on fix-written lines, the
   loop does not re-request. It asks the maintainer to choose between three options:
   - **Cut the scope.** The settled part stays in this pull request. The part the late findings
     keep landing on leaves it and becomes a backlog item for a decision of its own. Then
     one review is requested on the cut head, and the count starts again from that head as
     the baseline.
   - **Rewrite.** The affected section is rewritten in one piece, from rules that settle the
     neighbouring cases together. Then one review is requested, and the count starts again
     from the rewritten head as the baseline.
   - **Continue.** The loop re-requests as usual, and the count starts again with the same
     baseline, since the text was not rewritten.
   If half or fewer are on fix-written lines, the loop re-requests as usual and checks again
   after the next round with findings. After any of the three options, the next check comes
   after eight more rounds with findings.
3. **While it waits.** The pull request stays a draft and gets no review request until the
   maintainer answers. A loop with no maintainer present (for example `backlog-loop`) waits the
   same way, as for any other decision only the maintainer can make.
4. **Other pull requests.** Code pull requests and the rest of the loop are unchanged. ADR-251's
   re-request conditions, and the skill's single re-request after a round that changed no file,
   still decide whether a round re-requests at all; this pause only withholds a re-request they
   would allow.
5. **What the reviewer is told.** The review guidelines (`.github/copilot-instructions.md`, which
   Codex reads through `AGENTS.md`) say that a review requested after a cut or a rewrite counts
   as a first review, in which the reviewer reports everything it finds, and that after a cut
   the part moved to a backlog item is out of scope.

## Consequences

- A specification loop has a point where it stops by default, so the maintainer sees a
  run-away loop after about eight rounds instead of after dozens.
- Cutting the scope merges a specification whose open part is listed as a backlog item, not
  settled. Later work on that part goes through its own pull request and its own review.
- The blame count is a proxy. A fix that only re-wraps a paragraph makes its lines count as
  fix-written, so the share can come out high when the issue is older. The maintainer sees the
  share and the findings before choosing, and can choose Continue.
- Eight rounds and four rounds are a first setting. If the pause comes too early or too late,
  change the numbers here and in the skill.
