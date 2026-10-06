---
applyTo: "**/*.md,api/**,schemas/**,db/**"
---

# Review guidelines: documentation, API descriptions and schemas

Flag as P1:

- **Naming**: documentation not in English; a new documentation file (under `docs/`, a crate's
  `docs/`, or anywhere else) whose name is not kebab-case. Exceptions: ADRs
  (`ADR-{NNN}-{short-slug}.md`, plus `INDEX.md` and `TEMPLATE.md` in `docs/adr/`) and
  conventional upper-case files such as `README.md`.
- **Design-document section numbers**: renumbering or removing a section of
  `docs/system-architecture.md`. Code and docs cite it by number.
- **Schemas**: a change to `schemas/*.schema.json` without the matching `*.example.json`.
- **ADR format**: a new ADR that does not follow `docs/adr/TEMPLATE.md`; a superseded ADR whose
  `**Status:**` line was not updated (`Superseded by ADR-{NNN}`, or an annotation for a partial
  supersession).
- **Rule drift**: a change to a rule in `CLAUDE.md` without the matching change to the review
  guidelines in `.github/copilot-instructions.md` or `.github/instructions/`.
