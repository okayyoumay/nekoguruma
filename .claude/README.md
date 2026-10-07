# Claude Code configuration for Nekoguruma

This directory configures Claude Code for this workspace: project agents (subagents), project
skills, permissions and hooks. Repository rules (temporary vs. permanent files, documentation
sync, ADRs, spec copyright, pull requests) are in the root [CLAUDE.md](../CLAUDE.md); this file
covers how sessions split work between agents and keep cost down.

## Session model

Every agent pins its model, so the pipeline behaves the same whatever model the main session
runs. Sonnet is enough for every orchestration duty here (classifying a task, writing briefs,
reviewing agent reports, deciding). The expensive judgment steps are pinned to `edge-case-hunter`
(Opus) and `design-advisor` (Fable), so a top-tier session model adds cost rather than
capability unless the user asks for one.

## Agents

| Agent | Model (effort) | Tools | Use for |
|---|---|---|---|
| `code-scout` | haiku | read-only | Fan-out searches across many crates or docs; returns conclusions and `path:line`, never file dumps |
| `cargo-runner` | haiku | Bash + read-only | Builds, tests, clippy and fmt with long output; returns a compact pass/fail summary. Never edits files |
| `doc-sync-checker` | sonnet (medium) | Bash (git read) + read-only | Pre-commit audit of the diff against CLAUDE.md's documentation-sync table, the `work/` rule and the spec copyright rule |
| `scope-shaper` | sonnet (high) | read-only | Turns a fuzzy or oversized request into a minimal scope with acceptance criteria |
| `implementer` | sonnet (medium) | Bash + edit | Code, tests and doc edits from a distilled brief |
| `edge-case-hunter` | opus (high) | Bash + read-only | Verification pass before commit: boundaries, error paths, concurrency, protocol corner cases, interruption/resume, missing tests |
| `design-advisor` | fable (xhigh) | Bash (git read) + read-only | Escalation for decisions that are expensive to get wrong |

Each tier is used for what it does best, following the official model descriptions
([model overview](https://platform.claude.com/docs/en/about-claude/models/overview),
[Claude Code model configuration](https://code.claude.com/docs/en/model-config)); re-check them
when a new model generation ships, since the price ratios between tiers move:

- **Haiku** (`code-scout`, `cargo-runner`): mechanical work with a short report (locating code,
  running commands). It is the cheapest tier, has a 200K context window and carries no
  `effort` key (unsupported there), so it gets the agents whose output needs no judgment.
- **Sonnet** (`scope-shaper`, `implementer`, `doc-sync-checker`): everyday reasoning at a
  quarter of the top tier's price. `implementer` runs at `medium`, which fits implementation
  from a distilled brief; `scope-shaper` at `high` for the scoping judgment. `doc-sync-checker`
  runs here rather than on Haiku because its audit is judgment (is an ADR needed, does a
  sentence read like spec wording, does a document update really cover the change) over a whole
  PR diff plus spec searches, which also needs the larger context window; `medium` is enough
  for a checklist audit.
- **Opus** (`edge-case-hunter`): the verification pass runs only on gated diffs (protocol
  semantics, concurrency, FFI, trust boundaries), where a missed case costs more than the
  call. Opus at `high` reasons more reliably about reachability and ordering than Sonnet at
  `xhigh`, and the current Opus costs about twice Sonnet, not five times as earlier
  generations did.
- **Fable** (`design-advisor`): the top tier, which the official guidance aims at root-cause
  investigations and architecture decisions. It is reserved for the gated escalation path and
  runs at `xhigh`: it is spawned rarely, with a distilled brief, and depth is what the call pays
  for (requirement interpretation, cross-crate design, ADR-worthy choices). The effort is pinned
  so a session-level override cannot move the costliest path.

Custom agents load `CLAUDE.md` into their context at every spawn. `code-scout` and
`cargo-runner` set `omitClaudeMd: true`, since their prompts carry everything they need and
they are spawned most often. The two Haiku agents and `doc-sync-checker` set `maxTurns` as a
guard against a runaway search or polling loop; an agent that reaches it returns a partial
report.

## Task size decides the pipeline

Every agent spawn starts cold and re-reads context, so match the pipeline to the task:

1. **Small** (one file, or a question answered by one or two targeted reads): do it inline. No
   agents.
2. **Medium** (a few files in one crate, clear scope): the main session plans and reviews;
   `implementer` edits, `cargo-runner` verifies, `doc-sync-checker` audits before commit.
3. **Large** (several crates, a new subsystem, or unclear scope): as Medium, plus `scope-shaper`
   first when the request is genuinely fuzzy, `code-scout` for the fan-out search, and
   `edge-case-hunter` before commit.

In Medium and Large tasks the main session orchestrates: it spends its own tokens on planning,
reviewing agent output and final decisions, and uses tools directly only for what cannot be
delegated (the final commit and push) or a single targeted read to check an agent's claim.

## Gates for the analysis agents

- **`scope-shaper`** only when the request is fuzzy or oversized *and* pinning it needs several
  files. If one targeted read pins the scope, do that instead.
- **`edge-case-hunter`** after implementation, before commit, when the diff touches any of:
  protocol semantics (UDS, J2534, D-PDU API, COMPARAM mapping), concurrency or state machines
  (job state transitions, VM resume, worker lifecycle), FFI and `*-sys` crates, ABI handling,
  signatures and trust boundaries (`shared-crypto`, token handling), or more than one crate.
  Mechanical single-crate changes skip it.
- **`design-advisor`** when a wrong decision is expensive to reverse: interpreting ISO 22900 /
  J2534 / UDS requirements, concurrency or state-machine design across crates, trust-boundary
  and signature design, a choice that needs an ADR, or a bug that survived a `code-scout` +
  `cargo-runner` investigation. Hand it a distilled brief with the facts already collected, not
  an exploration task.

## Working rules

- **Batch, don't re-spawn.** Put related sub-questions into one agent call.
- **Resume for follow-ups.** A follow-up question to an agent that already did the reading
  goes to that agent with `SendMessage`: it keeps its context and prompt cache, where a new
  spawn re-reads everything. (Built-in `Explore` and `Plan` cannot be resumed.)
- **Bounded replies.** Every agent caps its report size; raw logs and file bodies stay out of the
  main conversation.
- **No speculative expensive builds.** Cross-target builds, `--features bindgen` and full
  release builds run only when explicitly requested; CI covers the target matrix.
- **One writer per file.** Never run two `implementer`s on the same file at the same time.
- **Check agent claims.** A claim that decides the outcome (a test passed, a file has no other
  callers, the spec requires X) is checked with one targeted read or command before it is
  relied on.

## Built-in agents

Claude Code also offers built-in agents. They are not pinned by this configuration, so route
around the expensive ones:

- **`Explore`** runs on the session model (Opus by default). For read-only searches, use
  `code-scout` (Haiku) instead; use `Explore` only when the user asks for it.
- **`Plan`, `general-purpose` and `claude`** use `CLAUDE_CODE_SUBAGENT_MODEL`, which
  `settings.json` sets to `sonnet`. Prefer a project agent when one fits the task.
- Forks (`/subtask`) inherit the session model and history. They reuse the parent's prompt
  cache, so they suit a short side task that needs the conversation's context; for anything
  that a brief can describe, a pinned agent is cheaper.

The project agents' `model:` keys take precedence over the environment variable, so the
pipeline above is unchanged by it.

## Skills

Project skills live in `.claude/skills/`:

| Skill | Use for |
|---|---|
| `adr-number-reservation` | Reserving an ADR number before writing the ADR |
| `unattended-clarification` | Asking a question when no user may be watching (scheduled or triggered runs) |
| `backlog` | Adding, updating and closing backlog items in `work/` |
| `next-task` | Recommending the next item to work on (one pick plus one alternative) |
| `backlog-triage` | Cleaning up the backlog; manual only (`/backlog-triage`) |
| `pr-review-loop` | Driving the Copilot review cycle on a PR Claude opened, up to handing it to the maintainer |
| `backlog-loop` | Working through the backlog one item and one PR at a time, waiting for the maintainer's merge between items; manual only |

Skills run in the main conversation at the session model. Where an agent covers the same ground
(build/test sweeps, doc-sync audits, verification), prefer the agent.
A skill that only reads a lot and reports a short result runs forked instead (`context: fork`
with a pinned `model`): `next-task` reads the whole backlog in a read-only Sonnet fork, so the
backlog files never enter the main conversation.

## Permissions and hooks

`settings.json` sets `CLAUDE_CODE_SUBAGENT_MODEL=sonnet` for the unpinned built-in agents (see
"Built-in agents") and pre-allows read-only git commands, the standard cargo verbs (check, build, test,
clippy, metadata, tree, and `fmt --check` only; plain `cargo fmt` rewrites sources, so it
prompts) and the repository check scripts (`check-work-refs.sh`, `check-adr-index.sh`,
`check-backlog.sh`, `classify-pr-risk.sh`). Anything that mutates state outside `target/` still
prompts.

`ask` rules make force pushes and `git reset --hard` prompt even in auto mode, and `deny` rules
keep `.env` files and private keys (`*.pem`, `*.key`) out of Claude's reads. `target/` is
denied too: build output is large and never the source of truth, and the read tools (`Read`,
`Grep`, `Glob`) would otherwise pull it into context. Personal
overrides go in `.claude/settings.local.json` (gitignored).

Hooks:

- **PostToolUse** (`Edit` / `MultiEdit` / `Write`): `scripts/check-work-refs.sh --hook` checks the
  edited file. If a permanent file names a file inside `work/`, the hook reports it back to
  Claude (exit 2) so it is fixed before commit.
- **SessionStart** (`.claude/hooks/session-start.sh`): after a context compaction it re-injects
  the four rules most easily lost from a summary. In Claude Code cloud sessions
  (`CLAUDE_CODE_REMOTE=true`) it also runs `cargo fetch` and `cargo check --workspace
  --all-targets` on startup, so the session starts with dependencies and a warm build cache. It
  runs synchronously (about a minute on a cold container).

## Path-scoped rules

`.claude/rules/*.md` files carry `paths:` frontmatter and load only when Claude reads or edits a
matching file. Put guidance that applies to one area there rather than in `CLAUDE.md`, which
loads into every session and should stay under about 200 lines.

## Changing this configuration

When a task runs into a real gap in this pipeline (a missing routing case, an agent behaving
badly, a rule that produced a bad outcome), propose the change to the user with the evidence
from that task. Do not edit `.claude/` or `CLAUDE.md` on your own initiative; these files govern
every later session.

`/doctor prompt-audit` checks the instruction files (CLAUDE.md, rules, agents, skills) for
outdated references and contradictions; run it after larger changes to this configuration.
