# ADR-490: Consult Modern Web Guidance for Browser Code

**Date:** 2026-11-17
**Status:** Accepted
**Affects:** `.claude/rules/web-ui.md`, `CLAUDE.md` ("Path-scoped rules"); design 9.6

## Context

M4 builds the Web UI (design 9.6, R4: acquired data is viewed and edited in an ordinary browser
without installation). Coding agents tend to write browser code from older patterns in their
training data: heavy JavaScript for what the platform now does natively, polyfills that are no
longer needed, and components that miss built-in accessibility.

Modern Web Guidance (`GoogleChrome/modern-web-guidance`, stewarded by the Chrome, Edge, Firefox
and MDN teams; code Apache-2.0, guides CC-BY 4.0; 1.0 released 2026-11-10) packages curated
guides on current web APIs, accessibility, performance and fallbacks behind a small CLI. An
agent searches for a guide offline and retrieves it into its context. The npm package has no
dependencies and no install scripts. By default it reports anonymous usage, including the
agent's search queries, to Google, and it is usually run as `npx ...@latest`.

No browser code exists in the repository yet.

## Decision

1. Agents consult Modern Web Guidance before writing or changing HTML, CSS or client-side
   JavaScript/TypeScript. A path-scoped rule (`.claude/rules/web-ui.md`) carries the
   instruction, so it loads only when such a file is read or edited and costs nothing in
   sessions that never touch browser code.
2. The rule calls the CLI through `npx` with a pinned version, not `@latest`, so a new upstream
   release cannot change what runs without a reviewed pull request. Raising the pin is its own
   pull request.
3. Telemetry is turned off on every call (`DISABLE_TELEMETRY=1`), and queries name a generic UI
   capability, never vehicle, customer or job data.
4. The Claude Code plugin is not installed or enabled in `.claude/settings.json`. It would add
   its skill to every session and update itself outside review; the rule gives the same
   guidance where it is needed.
5. Until the design names a browser support target, browser code aims at the tool's default
   (Baseline Widely available), and a pull request says when it relies on anything newer.
6. Guides are cited by ID and paraphrased; their text is not copied into the repository.

## Consequences

- Sessions that edit browser code need Node.js and network access to the npm registry for the
  first `npx` call; the rule's commands fail visibly when either is missing.
- The browser support target is still open; the M4 UI framework choice (design 9.6) should
  settle it, after which the rule follows the design.
- The rule's globs match any HTML, CSS or JavaScript/TypeScript file, including generated or
  vendored ones if any are added later; such files are not edited by hand anyway.
- Reviewers (Codex, Copilot) are not pointed at the guidance; they keep reviewing against
  `.github/copilot-instructions.md`.
