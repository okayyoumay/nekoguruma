# AGENTS.md

Instructions for Codex (and other agents that read `AGENTS.md`) working in this repository. The
full repository rules are in [CLAUDE.md](CLAUDE.md); this file restates the parts an agent needs
and adds the guidelines Codex follows when it reviews a pull request.

## Project

**Nekoguruma (NGR)** is diagnostic software for vehicles: a server, an agent on the device that
talks to the vehicle interface (VCI), and worker processes that load vendor J2534 / ISO 22900
D-PDU API libraries for the ABI they were built for. It is a Rust workspace with its crates under
`crates/`.

- [README.md](README.md): workspace table (crate, role, design-document section)
- [docs/system-architecture.md](docs/system-architecture.md): the design document. Code and docs
  cite it by section number (e.g. "7.3"), so its section numbers stay stable
- [docs/worker-crates.md](docs/worker-crates.md): worker crates, target ABIs and bindings
- [docs/adr/INDEX.md](docs/adr/INDEX.md): Architecture Decision Records

Build and test commands are in CLAUDE.md ("Building and testing").

## Review guidelines

Codex code review reads this section. The repository is public and MIT-licensed, so review
comments are public too.

### What to flag as P0

- **Copied standard text.** ISO 22900-2, SAE J2534-1, SAE J2534-2 and ISO 14229-1 are
  copyrighted. Any passage that reads as copied from one of them, quoted or not, at any length,
  in code comments, `docs/`, ADRs, commit messages or the PR description. Citations must give
  the clause or section number and paraphrase. Do not quote standard text in your own review
  comments either; describe the requirement in your own words.
- **Secrets or private infrastructure** in committed files: credentials, keys, tokens, internal
  hostnames.
- **Vehicle-safety regressions**: a change that lets a write, flash or routine-control job run
  without the preconditions and safety guards of `docs/system-architecture.md` 5.5, 5.6 and 8.9,
  or that bypasses the authorization and approval levels of section 6.

### What to flag as P1

- **Correctness**: logic errors, panics reachable from external input (gRPC, Web API, vendor
  library return values), unchecked FFI results, lost or duplicated messages, races and
  deadlocks, resource leaks across worker restarts.
- **`unsafe` placement**: new `unsafe` code outside the `*-sys` crates and the safe wrapper
  crates (`iso22900`, `j2534-0404`). Services and everything above them call the wrappers.
- **Generated code out of step with its source**: the committed bindings must match their
  source in both directions.
  - Proto: a change to `crates/vci-service-interface/src/bindings/vci.service.rs` without the
    matching `service.proto` change (hand edit), or a `service.proto` change without the
    regenerated bindings in the same PR (stale bindings). Proto regeneration is never deferred.
  - FFI: a change to `crates/*-sys/src/bindings/*.rs` without a header change (hand edit), or a
    header change without regenerated bindings, unless the same PR records the deferred
    regeneration as a backlog item in `work/`; a note in the PR description alone is not
    enough.
- **Documentation out of sync**: the PR changes an area without updating the document CLAUDE.md's
  "Documentation sync" table assigns to it (for example a `service.proto` change without
  `docs/rpc-api-guide.md`, a server API change without `api/openapi.yaml`, a database change
  without a new file in `db/migrations/`). A doc that now contradicts the code is P1, and so is
  renumbering or removing a section of `docs/system-architecture.md`, which code and docs cite by
  number.
- **ADRs**: a non-obvious design decision (data structure, concurrency model, state machine,
  protocol interpretation, trust boundary) or a spec requirement that drives the code in a
  surprising way, introduced without an ADR; a new ADR missing its row in `docs/adr/INDEX.md` or
  its theme entry; a duplicate ADR number; or a change that contradicts an accepted ADR without
  superseding it or annotating its Status line. Plain bug fixes and refactors without behaviour
  change need no ADR.
- **Naming**: a new crate, binary or command without the `ngr` prefix; documentation not in
  English; a new file under `docs/` whose name is not kebab-case (ADRs use
  `ADR-{NNN}-{short-slug}.md`).
- **Temporary-file references**: a permanent file (anything outside `work/`) that names a
  specific file inside `work/`. Naming the folder itself or `work/README.md` is fine.
  `work/` is temporary working material, and open items belong there, not in permanent docs.
- **Warnings silenced the wrong way**: `#[allow(...)]` added for code that is unused only until
  later work lands. This repository uses `#[expect(..., reason = "...")]` so the attribute fails
  once the code is used.
- **Tests weakened**: a test skipped, ignored, deleted or loosened to make CI pass, or
  `#[serial]` removed from a test that shares process-global state.
- **Out-of-scope targets**: Windows workers target `*-pc-windows-gnullvm` only (ADR-227).
  Adding an MSVC worker target or MSVC-only build path is P1.
- **CI cost**: the goal of CI changes here is fewer GitHub Actions minutes, not shorter
  wall-clock time. Flag a workflow change that raises total minutes: new parallel jobs or matrix
  entries, cross-target release builds on pull requests, losing the docs-only skip, or dropping
  build caching. Also flag a change that lets a required check be skipped and so pass without
  running.

### What not to flag

- Formatting and lints that `cargo fmt` and `cargo clippy` already report.
- PR numbers, review rounds and agent names in existing comments and notes in the worker crates.
  They are provenance from earlier history. New text should not add references of that kind,
  though; flag those.
- `todo!()` placeholders or short `TODO` comments in code. Flag only a TODO list or "not done
  yet" section added to a permanent document.

### How to write findings

- Lead with the failure scenario: the input or state, and the wrong result.
- Point at the line in the current diff, and cite the design section or ADR when the finding
  rests on one.
- When a finding depends on what a standard requires and you cannot read the standard, still
  report it, but name the standard, edition and clause and say that the claim needs checking
  against the text; do not state the requirement as fact. ISO 22900-2 exists in a 2009 and a
  2022 edition; code and docs say which one they target.
- One finding per root cause. Do not repeat a finding already answered on the PR unless the new
  code reintroduces it.
- Write in English.
