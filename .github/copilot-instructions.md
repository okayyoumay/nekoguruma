# Review guidelines

Nekoguruma (NGR) is vehicle diagnostic software in Rust (crates under `crates/`). Repository rules
are in `CLAUDE.md`; the design document is `docs/system-architecture.md`, cited by section number.
The repository is MIT-licensed and will be public: write review comments as if already public.
More rules for Rust, documentation and CI files are in `.github/instructions/`. The skills in
`.claude/skills/` are workflows for the sessions that write pull requests (for example
`pr-review-loop` answers your findings); they are not review rules, so do not apply them when
reviewing.

## P0

- **Copied standard text.** ISO 22900-2, SAE J2534-1/-2, ISO 14229-1, ISO 14229-2,
  ISO 15765-2, ISO 22901-1 and ISO 17978 are copyrighted, as is any other standard. Flag any
  passage that reads as copied from one of them, quoted or not, at any length, anywhere:
  comments, strings, fixtures, data files, docs, ADRs, commit messages, the PR description.
  Citations give the clause number and paraphrase. Never quote standard text in your own
  comments.
- **Secrets or private infrastructure**: credentials, keys, tokens, internal hostnames.
- **Vehicle safety**: a write, flash or routine-control job that can run without the
  preconditions and guards of design 5.5, 5.6 and 8.9, or that bypasses the authorization and
  approval levels of section 6.

## P1

- **Correctness**: logic errors, panics reachable from external input (gRPC, Web API, vendor
  library return values), unchecked FFI results, lost or duplicated messages, races, deadlocks,
  leaks across worker restarts.
- **Documentation out of sync**: a change without the matching document update in the same
  pull request. A doc that contradicts the code, or a renumbered or removed section of
  `docs/system-architecture.md`, is P1. The mapping (the same as `CLAUDE.md`'s "Documentation
  sync" table):

  | Change | Document |
  |---|---|
  | Design, behaviour, data flow, security model | `docs/system-architecture.md` |
  | Crate added, removed, renamed or re-scoped | `README.md` workspace table, and `docs/worker-crates.md` for worker crates |
  | Worker gRPC interface (`crates/vci-service-interface/src/proto/service.proto`) | `docs/rpc-api-guide.md` |
  | J2534 v04.04 adapter (`j2534-0404-service`) | `docs/j2534-0404-architecture.md`, `docs/j2534-2-support-plan.md` |
  | Server Web API / event stream | `api/openapi.yaml`, `api/asyncapi.yaml` |
  | JSON schemas | `schemas/*.schema.json` and the matching `*.example.json` |
  | Database | a new file in `db/migrations/`, plus `db/README.md` |
  | New domain term | `docs/glossary.md` |
  | Crate scope, policy or detailed design | that crate's `crates/<crate>/docs/*.md` |
  | Non-obvious design decision | a new ADR (see the ADR rule below) |
- **ADRs**: a non-obvious decision (data structure, concurrency model, state machine, protocol
  interpretation, trust boundary) or a surprising spec-driven behaviour without an ADR; a new
  ADR without its `docs/adr/INDEX.md` row and theme entry (one line per ADR, in numeric order); an `INDEX.md` merge-conflict resolution that drops either side's line or breaks numeric order (such conflicts between ADR pull requests are accepted, not a finding in themselves); a duplicate ADR number; a change that
  contradicts an accepted ADR without superseding it or annotating its Status line.
- **Spec citations**, anywhere (code, docs, scripts, commit messages, the PR description): a
  standard cited without its clause or section number, or an ISO 22900-2 citation that does not
  say whether it targets the 2009 or the 2022 edition.
- **Naming**, in any file type: a new crate, binary or command without the `ngr` prefix; a new
  path or configuration directory named after the project that does not use the full lowercase
  name `nekoguruma` (for example `ngr/` or `Nekoguruma/`); documentation not in English; a new
  documentation file, in any format, whose name is not kebab-case. Exceptions: ADRs
  (`ADR-{NNN}-{short-slug}.md`, plus `INDEX.md` and `TEMPLATE.md` in `docs/adr/`) and
  conventional upper-case files such as `README.md`.
- **Temporary-file references**: a file outside `work/` naming a specific file inside `work/`
  (the folder itself and `work/README.md` are fine). Open items belong in `work/`.
- **Tests weakened**: a test skipped, ignored, deleted or loosened to pass CI, or `#[serial]`
  removed from a test sharing process-global state.

## Do not flag

- What `cargo fmt` and `cargo clippy` report.
- PR numbers, review rounds and agent names in older ADRs, notes and code comments, including
  the worker crates' (provenance from earlier history). Do flag new text that adds such
  references.
- `todo!()` or short `TODO` comments in code. Flag only a TODO list added to a permanent doc.

## Writing findings

- Lead with the failure scenario: the input or state, and the wrong result.
- Point at the line in the diff; cite the design section or ADR the finding rests on.
- If a finding depends on a standard you cannot read, name the standard, edition and clause
  and say the claim needs checking; do not state the requirement as fact.
- One finding per root cause. Do not repeat a finding already answered on the PR unless new
  code reintroduces it.
- Report everything you find in your first review of a pull request; do not hold findings back
  for a later round.
- On a re-review, report findings in code changed since your previous review, and P0 or P1
  findings anywhere. Do not raise new low-severity findings on code that has not changed since
  your previous review; if you notice any, give only their number in the summary. Each re-review
  is requested on purpose and spends the owner's AI credits, so it should converge.
- Write in English.
