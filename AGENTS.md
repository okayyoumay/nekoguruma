# AGENTS.md

Nekoguruma (NGR) is vehicle diagnostic software in Rust (crates under `crates/`). Repository rules
are in `CLAUDE.md`; the design document is `docs/system-architecture.md`, cited by section number.

## Review guidelines

The review guidelines for this repository are kept in one place for every code reviewer: follow
`.github/copilot-instructions.md` for every pull request, and in addition each file in
`.github/instructions/` whose `applyTo` globs match a file the pull request changes. Read them from
the pull request's head branch. They are not repeated here, so that the reviewers cannot drift
apart.

Two terms in those files map onto your own review format:

- Their "P0" and "P1" lists name what you badge P0 and P1. Badge anything else P2, or P3 when it
  is minor (wording, style, an unlikely edge case with a small impact).
- Where they say a finding is rated "Medium or higher", read P2 or higher; "Low" is P3.
