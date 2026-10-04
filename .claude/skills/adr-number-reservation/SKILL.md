---
name: adr-number-reservation
description: Atomically reserve a unique ADR number before writing any ADR in nekoguruma by creating the adr-reservation/{NNN} branch with the GitHub MCP create_branch tool (422 on collision means taken, so increment and retry). Use BEFORE creating an ADR file, adding its INDEX.md row, or writing its number into code or docs, including ADRs that supersede or amend earlier ones.
---

# ADR number reservation

Two parallel sessions computing "max + 1 over `docs/adr/`" pick the same
number, and the duplicate merges silently: the files have different slugs, so
git never conflicts on them. ADR numbers are cited from code and documents,
so a merged duplicate cannot simply be renumbered. The number has to be
exclusive before it is first written down.

## Step 1: compute the candidate

Take the maximum of these two, then add 1:

- the highest `ADR-{NNN}-*.md` on `origin/main` (run `git fetch origin main`
  first; your working tree may be stale);
- the highest existing `adr-reservation/{NNN}` branch
  (`git ls-remote --heads origin 'adr-reservation/*'`, or the GitHub MCP
  `list_branches` tool).

## Step 2: reserve it atomically

Create the reservation branch through the GitHub API, never with `git push`
(the session git relay only accepts the session's own branch):

- `mcp__github__create_branch` with owner `okayyoumay`, repo `nekoguruma`,
  `branch: adr-reservation/{NNN}`, `from_branch: main`.
- Success: the number is yours.
- `422 Reference already exists`: another session holds it. Increment and
  retry.

Right after a successful create, fetch `origin/main` again and confirm no
`docs/adr/ADR-{NNN}-*.md` exists there. The two reads in Step 1 are not
atomic, so an ADR may have merged in between. If the file exists, do not use
the number; recompute and retry.

Rules:

- `{NNN}` is the exact digit string the file name uses (three digits,
  zero-padded, until ADR-999).
- Never commit to the reservation branch; it is only a lock.
- One reservation per ADR.

## Step 3: write the ADR

Create `docs/adr/ADR-{NNN}-{short-slug}.md` from `docs/adr/TEMPLATE.md`, add
the `INDEX.md` row in numeric order and the theme-section entry, and run
`scripts/check-adr-index.sh`.

Parallel ADR pull requests still produce ordinary line conflicts in
`INDEX.md` (all append at the bottom of the table). Resolve them by keeping
both sides' rows in numeric order, then run `scripts/check-adr-index.sh`.

## Cleanup

- After merge: `.github/workflows/adr-reservation-cleanup.yml` deletes every
  reservation branch whose ADR is on `main`.
- If the ADR is abandoned: sessions cannot delete branches. Tell the user to
  delete `adr-reservation/{NNN}` on GitHub. Until then the number stays
  used; never reuse a number you did not reserve yourself.

## Fallback: GitHub MCP unavailable

Retry the tool once (ToolSearch can wait for a reconnecting server). If it is
still unavailable and the work must go ahead:

1. take max + 1 from a fresh `origin/main` fetch;
2. say prominently in the PR body that the number is UNRESERVED;
3. just before merge, fetch `origin/main` and list `adr-reservation/*`
   again; if the number was taken, renumber within your own diff.
