# ADR-163: Conformance audit doc — per-finding record is authoritative, Document Control is a checked index

**Date:** 2026-08-07
**Status:** Accepted
**Affects:** `j2534-0404-service/docs/iso22900-2-conformance-audit.md`, `scripts/ci-checks.sh`, `.github/workflows/` (CI invokes `scripts/ci-checks.sh`)

## Context

`iso22900-2-conformance-audit.md` records roughly 60 individual conformance
findings. Each finding is its own subsection (Part A) or table row (Part B)
stating its disposition (fixed / verified / reclassified, with the deciding
ADR). The document also has a "Document Control" section whose resolved-items
list restates the same disposition, one line per finding ID, so a reader can
scan aggregate status without opening every finding.

Across PRs #38–#40, five separate Codex review rounds each found the same
underlying defect in a new location: this document's status was asserted in
more than one place, and the copies drifted. PR #40 rounds 1–3 fixed
document-level restatements (a Part A heading note, a "Recommended next
steps" list, and matching copies in `docs/TODO.md` and this package's
`implementation-notes.md`) by adopting a "single source of truth" convention:
only Document Control's resolved-items list may state a finding's
disposition, and every other section must be a bare pointer to it.

Round 5 found that convention's own claim was false: each finding's own
heading (Part A) or triage cell (Part B) *also* independently states
disposition, and a "sole place states disposition" invariant can never
literally hold for this document — a FIXED finding's body is inherently a
fix narrative, and Part B's triage column embeds its own `**FIXED (ADR-N)**`
markers. Investigating this finding surfaced a live instance of exactly the
drift it warned about: Part B item B26's triage cell claimed finding A2-24
"remains a separate, still-open gap," but A2-24's own heading already read
`FIXED (ADR-118)`, and `docs/adr/ADR-118-cyclic-cop-status-events.md`
confirms ADR-118 closed precisely the gap ADR-117 (B26's fix) deferred. That
staleness was a second-order copy — one finding's record restating a
*different* finding's disposition — a variant the round 1–3 fixes never
targeted because they only addressed document-level meta-sections, not
inter-finding cross-references.

A convention phrased as "only one place may say this" cannot survive contact
with a document whose primary content — each finding's own detailed record —
is inherently a disposition statement. Textual discipline alone has now
failed five times on this exact document; a sixth prose-only fix is not
expected to hold either.

## Decision

Invert the model instead of continuing to narrow the ban:

1. **Each finding's own record is the authoritative source for its
   disposition.** For a Part A finding, that is its heading plus body. For a
   Part B item, that is its table row (the `ADR` and `Suggested triage`
   cells). No other location may independently assert what these already
   state.
2. **Document Control's resolved-items list is a derived index, not a second
   source.** It exists for aggregate scanning (and, for a reclassified Part A
   item, the Part A ID → Part B ID mapping) and must always agree with the
   records in (1). This is the same duplication-with-enforcement pattern
   `docs/adr/INDEX.md` already uses against the ADR files themselves — the
   duplication is deliberate, and `scripts/ci-checks.sh adr-check` (ADR-119)
   catches drift instead of prose asserting there is none. A matching
   `audit-doc-check` subcommand is added to `scripts/ci-checks.sh`: it
   extracts `(ID, disposition, Part-B-target)` triples from Part A headings
   and Part B triage cells, extracts the same from Document Control's roster,
   and fails on any triple present in one set but not the other, plus a
   referential-integrity check that every Part-B-target actually names a real
   Part B row. Every roster line carries only its ID, disposition token, and
   (for a reclassified item) its `(Part B/BXX)` target — no ADR reference, no
   verification annotation, no other detail of any kind, for any disposition,
   since that detail already lives at the finding's own record and the
   check never compared it: an earlier version of this rule exempted only
   reclassified entries, letting FIXED/VERIFIED roster lines keep an ADR
   reference as an unchecked "convenience" copy — a PR #40 review round
   proved that copy could drift silently (changing a roster ADR reference to
   a nonexistent one still passed), so the exemption for FIXED/VERIFIED
   entries was removed rather than teaching the check to compare one more
   field, on the expectation that comparing more fields just relocates the
   next uncompared scrap rather than eliminating the class of gap.
3. **All other locations state no per-finding disposition, only pointers.**
   This carries forward PR #40 rounds 1–3's fix unchanged: the Status line,
   the Part A heading note, "Recommended next steps", `docs/TODO.md`, and
   this package's `implementation-notes.md` each point at Document Control (or
   at a specific finding) rather than restate status.
4. **A cross-reference from one finding's record to another finding's record
   is a bare pointer, never a restatement.** When A2-24's note references
   B26, or B26's cell references A2-24, or a reclassified Part A item's own
   heading references its Part B target, it says "see BXX" / "see AX-Y" and
   nothing about that target's current disposition — that disposition is
   BXX's/AX-Y's own record's job to state, per (1). This is the rule the live
   B26/A2-24 staleness violated, and the rule (1)–(3) alone do not cover,
   since neither location is Document Control.
5. **No location states an aggregate free-text completion claim about Part A
   as a whole ("fully resolved", "N% resolved", or any equivalent), and
   `audit-doc-check` does not attempt to validate one.** The Status line
   carried such a claim from PR #40 round 6 onward, gated by a check that
   grepped for the literal substring `fully resolved`. Four consecutive
   Codex review rounds on that same PR each defeated the check from a
   different angle without ever falsifying the underlying claim: a
   Markdown line-wrap splitting the phrase across a line break, a second,
   independently-worded aggregate sentence elsewhere in Document Control
   the check never parsed, the reverse direction (every Part A finding
   resolved but the Status line not updated to say so), and finally a
   synonym rewording (`all resolved` for `fully resolved`) that changed
   nothing the check actually looked for. Each fix in turn widened the
   check's pattern-matching surface (join-and-collapse whitespace, a
   second grep, a second direction) rather than closing the underlying
   gap, because the gap is not a missing pattern — it is that validating
   arbitrary natural-language paraphrase of a semantic claim via
   substring/regex matching has no finite set of phrasings to enumerate.
   Per the same "eliminate rather than widen" principle Decision item 2
   above already applied to unchecked roster detail, the claim itself was
   removed from the Status line's prose rather than chased through a
   fifth phrasing, and the now-pointless check block was deleted from
   `audit-doc-check` rather than patched again. The Status line now states
   only that it makes no aggregate claim and points to each Part A
   finding's own heading and to Document Control's checked roster — both
   already covered by rule (1)/(2) above — as the place to look instead.

## Consequences

- Duplication is bounded to exactly one checked pair per finding (its own
  record, and Document Control's index entry), mirroring the
  `docs/adr/INDEX.md` / ADR-file pattern this repo already trusts.
- Reopening a finding or changing its disposition now fails
  `scripts/ci-checks.sh audit-doc-check` (and thus CI) unless both the
  finding's own record and its Document Control line move together — this
  audit doc's next round of active work is expected to exercise that gate
  for the first time.
- The check's parser is coupled to this document's current formatting
  conventions (heading disposition tokens, Part B triage-cell bold markers,
  roster line grammar). A future reformatting of any of these must update
  `audit-doc-check` in the same PR (ADR-155) or the check will false-fail
  on a correct document.
- The live B26/A2-24 staleness is corrected in the same PR as this ADR, as
  the first concrete instance of rule 4.
- Per rule 5, a reader of this document has no single free-text sentence
  claiming "Part A is done" — they must instead check Document Control's
  roster (or count Part A findings directly) to answer that question. This
  is a deliberate loss of at-a-glance summary convenience in exchange for
  the claim being impossible to let go stale, following the same
  substance-over-summary trade-off rule 2 already made for per-finding ADR
  references.
- This pattern is specific to this audit document's own structure; it is not
  a workspace-wide documentation-sync mechanism and does not replace
  `doc-sync-checker`'s existing checks.
