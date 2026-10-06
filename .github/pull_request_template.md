## Summary

<!--
Before: what a reader sees today. After: what they see with this change, and why.
Keep it self-contained: it is read without the conversation that produced the PR.
-->

Before:

After:

## Changes

-

## Documentation

<!-- Keep only the lines that apply and tick them. Source: CLAUDE.md's documentation-sync table. -->

- [ ] `docs/system-architecture.md` (design, behaviour, data flow, security model)
- [ ] `README.md` workspace table / `docs/worker-crates.md` (crate added, removed or re-scoped)
- [ ] `docs/rpc-api-guide.md` (worker gRPC interface)
- [ ] `docs/j2534-0404-architecture.md` / `docs/j2534-2-support-plan.md` (J2534 adapter)
- [ ] `api/`, `schemas/`, `db/` (server API, JSON schemas, database)
- [ ] `docs/glossary.md` (new terms)
- [ ] Crate docs under `crates/<crate>/docs/`
- [ ] New ADR and `docs/adr/INDEX.md` row
- [ ] No documentation change needed

## Checks

<!-- Tick each line, or say why it does not hold. The full rules are in `.github/copilot-instructions.md` and `.github/instructions/`. -->

- [ ] No standard text is copied anywhere in this PR, including commit messages and this description; standards are cited by clause or section number and paraphrased, and ISO 22900-2 citations name the 2009 or 2022 edition.
- [ ] No secrets or private infrastructure details (credentials, keys, tokens, internal hostnames or addresses).
- [ ] Generated code: proto bindings regenerated with `service.proto`; FFI bindings not hand-edited, and a deferred FFI regeneration is recorded in the backlog; a newly listed worker target has its `src/bindings/{target}.rs` in every `*-sys` crate. (N/A if none of these changed.)
- [ ] Write, flash and routine-control jobs keep the preconditions and safety guards of `docs/system-architecture.md` 5.5, 5.6 and 8.9, and the authorization and approval levels of section 6. (N/A if none of these changed.)

## Test plan

<!-- CI runs neither clippy nor the cross-target release builds on pull requests. -->

- [ ] `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --locked` clean locally
- [ ] `scripts/check-work-refs.sh`, `scripts/check-adr-index.sh`, `scripts/check-backlog.sh` pass
- [ ] CI green
- [ ] Other (describe):

## Open items

<!--
Backlog items this PR closes, and anything deferred with where it is recorded in work/.
Write "None" if nothing.
-->
