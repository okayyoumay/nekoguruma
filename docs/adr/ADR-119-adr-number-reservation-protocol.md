# ADR-119: Atomic ADR Number Reservation via `adr-reservation/{NNN}` Branches

**Date:** 2026-07-23 (brought into this repository on 2026-10-08, adapted to its tooling)
**Status:** Accepted
**Affects:** `docs/adr/` numbering process, `.claude/skills/adr-number-reservation/`, `scripts/check-adr-index.sh`, `.github/workflows/repo-checks.yml`, `.github/workflows/adr-reservation-cleanup.yml`, `CLAUDE.md`

## Context

ADR numbers were assigned by scanning `docs/adr/` for the highest existing number and adding
one. When several sessions work in parallel on separate branches, two in-flight PRs can pick the
same next number. The failure is silent: the two ADR files have different slugs, so git reports
no conflict on them and the duplicate number simply merges. The only textual collision is the
adjacent rows in `INDEX.md`, which git may or may not flag.

Renumbering after a duplicate has merged is not a realistic repair. Merged ADR numbers are cited
from code comments, documents and the supersession chains between ADRs. Renumbering an ADR that
has not merged yet touches only its own PR, but still costs a review round. A number therefore
has to be correct, and exclusive, the moment it is first written down.

Alternatives considered:

- **Renumber on collision, with CI detection only.** Detection, not prevention: every collision
  costs a renumber and a review round. Kept only as the fallback when reservation is impossible,
  with the CI check as a backstop for every path.
- **Tag-based reservation**, using the server's refusal to create an existing ref as the lock.
  The remote-session git relay accepts pushes only to the session's own branch, so sessions
  cannot push tags. Rejected.
- **Issue numbers as ADR numbers.** Atomic and available through the GitHub MCP server, but issue
  numbers share a counter with pull requests, so ADR numbers would jump and grow with PR volume.
  Rejected for breaking the contiguous numbering.
- **Date-based IDs.** No shared counter, but incompatible with the existing `ADR-{NNN}` files and
  their short cross-references. Rejected.

## Decision

1. **An ADR number is reserved, never computed.** Before an ADR file is created, the author
   reserves the number by creating the branch `adr-reservation/{NNN}` from `main` through the
   GitHub API (the GitHub MCP server's `create_branch` tool), not with a git push, which the
   session git relay rejects for other refs. Ref creation is atomic on the server: a second
   attempt fails with `422 Reference already exists`, and the author then increments and
   retries. Once the branch exists, the number is final and is never renumbered.
2. **The reservation branch points at `main` and never receives commits.** It is a lock, not a
   workspace, so deleting it later cannot lose any work.
3. **Reservation branches are deleted once the ADR merges.**
   `.github/workflows/adr-reservation-cleanup.yml` runs on pushes to `main` that touch
   `docs/adr/` and deletes every `adr-reservation/{NNN}` branch whose `ADR-{NNN}-*.md` file is
   on `main`. After each merge, the remaining reservation branches are the reservations still in
   flight. The workflow always evaluates `main`, also when started by hand, and it tolerates a
   branch that a concurrent run already deleted; any other deletion failure fails the job.
4. **A freed number is checked again before use.** The cleanup frees a reservation name as soon
   as its ADR merges, and a session reads `origin/main` and the list of reservation branches in
   two separate fetches. A number can therefore look free in both, although its ADR merged in
   between. Right after `create_branch` succeeds, the session fetches `origin/main` again and
   confirms that no `ADR-{NNN}-*.md` exists there. If one does, the number is abandoned (the
   stale branch matches the cleanup's pattern and is removed by the next `docs/adr/` push) and
   the session starts over.
5. **CI backstop.** `scripts/check-adr-index.sh`, run by `repo-checks.yml` on every pull
   request, fails on duplicate ADR numbers and on `INDEX.md` drifting from the directory. It
   catches any path that bypassed the reservation.
6. The exact procedure (candidate, retry, check after creation, cleanup, the fallback without
   the GitHub MCP server) is packaged in the `adr-number-reservation` skill, which `CLAUDE.md`
   points to.

## Consequences

- Parallel sessions can no longer take the same ADR number: the race is decided on the server at
  reservation time. Numbering is not guaranteed to stay contiguous: an abandoned reservation, or
  a number abandoned by the check after creation, leaves a gap that is never filled.
- Sessions cannot delete branches, so an abandoned reservation (an ADR that never merges) leaves
  a branch the cleanup never removes. A person deletes it on GitHub; until then the number stays
  used. The skill says to tell the user when this happens.
- Reservation needs the GitHub MCP server. Without it, the skill's fallback is max + 1, marked as
  unreserved in the PR, with a mandatory check before merge and the CI backstop.
- The check after creation costs one fetch per reservation. It narrows the race to the short
  window between that fetch and the ADR file being written; with the CI backstop, no duplicate
  can reach `main` undetected.
- `adr-reservation/*` branches appear in the branch list while ADRs are in flight. They carry no
  commits and are short-lived; CI does not run on them.
- Unlike the predecessor repository, this repository has no workflow that reserves a number
  without an agent; a person reserves one by creating the branch in the GitHub UI. The
  predecessor's version of this ADR therefore had a duty this one does not: keeping that
  workflow and the skill in sync. ADR-155's Status line and its Decision items 4 and 6 relax that
  duty (same pull request instead of same commit). Here only the skill implements the
  procedure, so the duty and ADR-155's relaxation of it do not apply.
