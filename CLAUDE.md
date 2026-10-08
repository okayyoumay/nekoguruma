# CLAUDE.md

Guidance for Claude Code sessions working in this repository. Agent usage, model allocation and
cost rules are in [.claude/README.md](.claude/README.md).

## Project

**Nekoguruma (NGR)** is diagnostic software for vehicles: a server, an agent on the device that
talks to the vehicle interface (VCI), and worker processes that load vendor J2534 / ISO 22900
D-PDU API libraries for the ABI they were built for. Start with:

- [README.md](README.md): workspace table (crate, role, design-document section)
- [docs/system-architecture.md](docs/system-architecture.md): the design document. Code and docs
  cite it by section number (e.g. "7.3")
- [docs/worker-crates.md](docs/worker-crates.md): the `iso22900*`, `j2534-0404*` and
  `vci-service-*` worker crates, target ABIs and bindings

Naming: the full lowercase name `nekoguruma` is used for paths and configuration directories;
`ngr` is the prefix for commands, binaries and new crates (e.g. `ngr-agent`).

## Guidelines

If an instruction contains a contradiction, ambiguity or missing information that changes the
outcome, ask before making changes. When a choice is reversible and the instruction leaves it
open, pick the reasonable default and say which one you picked.

### Temporary vs. permanent files

`work/` holds temporary working material (task lists, open items, status notes, the worker-crate
backlog). Its rules are in [work/README.md](work/README.md). In short:

- Open items and "not done yet" notes go in `work/`, never in permanent documents. A `todo!()`
  placeholder or short `TODO` comment in code is fine; list the work itself in `work/`.
- Permanent files (everything outside `work/`, including this file and `.claude/`) must not name
  files inside `work/` or depend on their numbering. Naming the folder itself, or
  `work/README.md`, is fine.
- When an item in `work/` produces a lasting decision, write the decision into the permanent
  document it belongs to (usually `docs/system-architecture.md`, a crate's docs, or an ADR), then
  delete the item. A finished item is deleted, not checked off.

`scripts/check-work-refs.sh` checks the second rule. It runs in CI and as a Claude Code hook
after every file edit (`.claude/settings.json`).

### Backlog

Open items live in the backlog files in `work/`, one prioritized bullet per item (`- **P1**: ...`).
The format, the P0-P3 scale and the file list are in `work/README.md` ("Backlog format");
`scripts/check-backlog.sh` checks it in CI. Use the skills:

- `backlog`: add an item whenever work is deferred, and close it (delete it, keeping any
  residual as a new item) when it is done
- `next-task`: recommend what to pick up next
- `backlog-triage` (run on request only): clean up stale, duplicate or mis-prioritized items
- `backlog-loop` (run on request only): work through the backlog one item and one pull request
  at a time; the next item starts only after the maintainer merges the previous PR

### Documentation sync

Update every document that describes the area you change, in the same pull request:

| Change | Document |
|---|---|
| Design, behaviour, data flow, security model | `docs/system-architecture.md` (keep its section numbers stable; code cites them) |
| Crate added, removed, renamed or re-scoped | `README.md` workspace table, and `docs/worker-crates.md` for worker crates |
| Worker gRPC interface (`crates/vci-service-interface/src/proto/service.proto`) | `docs/rpc-api-guide.md` |
| J2534 v04.04 adapter (`j2534-0404-service`) | `docs/j2534-0404-architecture.md`, `docs/j2534-2-support-plan.md` |
| Server Web API / event stream | `api/openapi.yaml`, `api/asyncapi.yaml` |
| JSON schemas | `schemas/*.schema.json` and the matching `*.example.json` |
| Database | a new file in `db/migrations/`, plus `db/README.md` |
| New domain term | `docs/glossary.md` |
| Crate scope, policy or detailed design | that crate's `crates/<crate>/docs/*.md` |
| Non-obvious design decision | a new ADR (below) |

Documentation is written in English and file names are kebab-case.

### Architecture Decision Records

ADRs live in `docs/adr/` as `ADR-{NNN}-{short-slug}.md` and follow
[docs/adr/TEMPLATE.md](docs/adr/TEMPLATE.md). Each new ADR gets a row in `docs/adr/INDEX.md`'s
main table and an entry under the matching theme section at the bottom of that file: one
`- ADR-{NNN}` line per ADR, in numeric order, with any status annotation after it in parentheses.

Write an ADR when a design choice is non-obvious (data structure, concurrency model, state
machine, protocol interpretation, trust boundary), when a prior ADR is revised, or when a spec
requirement drives the code in a way that would surprise a reader. Plain bug fixes and refactors
without behaviour change need none.

**Numbering:** reserve the number before writing it anywhere. Parallel sessions that compute
"max + 1" pick the same number, and the duplicate merges silently because the slugs differ. Use
the `adr-number-reservation` skill (it creates `adr-reservation/{NNN}` via the GitHub API).
`scripts/check-adr-index.sh` (CI) fails on duplicate numbers or `INDEX.md` drift.

**Index conflicts:** two pull requests that each add an ADR both append to the end of
`INDEX.md`'s table, and to the same theme list when they share a theme, so they conflict there.
This is accepted (the maintainer's decision; the index stays hand-maintained): resolve the
conflict when it appears by merging `main`, keeping both sides' lines in numeric order, and
running `scripts/check-adr-index.sh`.

When superseding an ADR, set its `**Status:**` to `Superseded by ADR-{NNN}`; for a partial
supersession, annotate instead, e.g. `Accepted (Decision item 2 superseded by ADR-{NNN})`.

Older ADRs, notes and code comments in the worker crates record their provenance (PR numbers,
review rounds, agent names). Those refer to earlier history that is not part of this repository;
read them as provenance, and do not cite such history in new text.

### Spec references and copyright

ISO 22900-2 (2009 and 2022 editions), SAE J2534-1 (v04.04), SAE J2534-2, ISO 14229-1 (2026
edition), ISO 14229-2 (2021), ISO 15765-2 (2024), ISO 22901-1 (2008) and ISO 17978-1/-2/-3
(2026) are available as converted text in the sibling `vehicle-comm-specs` repository (add it
to the session with `add_repo` if it is missing). Check which edition a finding targets before citing it.

These standards, and any other standard, are copyrighted. **Never copy their text verbatim**, at
any length, into anything this repository stores: code comments, `docs/`, ADRs, commit messages, PR or issue bodies. Cite
the clause or section number and paraphrase in your own words.

### Generated code

Proto bindings (`crates/vci-service-interface`) and FFI bindings (`crates/*-sys/src/bindings/`)
are generated and committed. Never hand-edit them, and never regenerate FFI bindings without
asking first (slow cross builds). The details are in `.claude/rules/generated-code.md`, which
loads when you open files in those crates.

### Path-scoped rules

Rules that matter only for part of the repository live in `.claude/rules/` and load when Claude
reads or edits a matching file: `generated-code.md`, `worker-crates.md`, `work-folder.md`.
Personal, uncommitted instructions go in `CLAUDE.local.md` (gitignored).

## Building and testing

```sh
cargo check --workspace --locked
cargo test --workspace --locked --exclude sim-vci
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked
scripts/check-work-refs.sh
scripts/check-adr-index.sh
scripts/check-backlog.sh
```

CI runs the tests with [cargo-nextest](https://nexte.st/) instead. The `core-linux` job runs
`cargo nextest run --workspace --locked --exclude sim-vci --profile ci`, then
the doc tests with `cargo test ... --doc`. nextest runs each test in its own process, so the
`#[serial]` worker-crate integration tests run in parallel there (`.config/nextest.toml`).
`cargo test` gives the same results, only more slowly; keep `#[serial]` on tests that share
process-global state, since `cargo test` still needs it. Some of those tests are too
timing-sensitive to run in parallel on the Windows runner, so the `core-windows` job uses the
`ci-windows` profile (one test at a time). On pull requests `core-windows` leaves out
`j2534-0404-service`, which has no Windows-specific code; main runs every test on Windows.

CI (`.github/workflows/ci.yml`) also checks the worker crates for six targets and runs
`scripts/abi-roundtrip.sh`, which launches `j2534-0404-service` against `sim-vci` built in debug
for a worker target (Linux ARM under qemu-user, Windows on a Windows runner) with the ABI table's
`unsigned long` width. On pull requests the six worker targets are type-checked (`worker-check`,
`cargo check --target`) and `abi-roundtrip` launches the i686 and aarch64 builds. main launches all
four Linux targets, runs the release builds (cargo-zigbuild for Linux, llvm-mingw for the
`*-pc-windows-gnullvm` Windows targets, all on Linux), and launches the Windows debug builds that
`worker-windows` cross-compiles (`abi-roundtrip-windows`). On pull requests that change only documentation, `work/` or `.claude/`, the
`changes` job in `ci.yml` skips the build jobs, which then count as passed required checks;
`repo-checks.yml` always runs. Do not run cross-target builds locally unless asked; CI covers
them.

Compiler warnings fail CI: `core-linux`, `core-windows` and `worker-check` build with
`RUSTFLAGS=-D warnings`, and `core-linux` runs `cargo fmt --check`. Clippy is not run in CI
(it would add a build to every pull request); run it locally before pushing. Silence a warning for code that
is unused only until later work lands with `#[expect(..., reason = "...")]`, not `#[allow]`, so
the attribute fails the build once the code is used and gets removed.

## Pull requests

1. Work on the branch you were given; open the pull request as a draft and fill in
   `.github/pull_request_template.md`.
2. Drive CI to green. A failure is fixed at its root cause; never skip, disable or loosen a test
   to get green.
3. Run the automated review loop with the `pr-review-loop` skill: request a review from GitHub
   Copilot, fix or answer every finding, and re-request until a round is clean. If Copilot code
   review is not available, or the owner's Copilot budget is used up, the skill falls back to an
   `edge-case-hunter` pass over the whole diff and tells the maintainer.
4. Before marking the PR ready, check that nothing deferred during the work exists only in the
   conversation: every follow-up is either done in the PR or added to the backlog (`backlog`
   skill). Items the PR finishes are closed in the same PR.
5. The maintainer (the repository owner) reviews and merges. Do not merge your own PR unless asked.

Commit messages and PR descriptions follow the same copyright rule as the code.

Copilot reviews against [.github/copilot-instructions.md](.github/copilot-instructions.md) and
the path-specific files in `.github/instructions/`, which restate the rules above as review
priorities; a path-specific file applies only to the files its `applyTo` globs match. When a
rule in this file changes, update those files in the same pull request; the "Documentation sync"
table is copied into `.github/copilot-instructions.md`. Copilot code review also reads the skills
in `.claude/skills/` when it judges them relevant, so the repository-wide file tells it that those
skills are not review rules.
