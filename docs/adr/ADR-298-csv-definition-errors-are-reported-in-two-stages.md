# ADR-298: CSV Definition Errors Are Reported in Two Stages

**Date:** 2026-10-14
**Status:** Accepted
**Affects:** `diag-frontend` (`src/tables/`, `docs/csv-tables.md` section 7); amends ADR-295 item 11

## Context

ADR-295 item 11 says the CSV conversion lists every error at once. The reader in
`diag_frontend::tables` checks each file on its own: the file set, the record format, the
header, each cell, and names defined twice in one table. A row with such an error has no typed
value. The next checks compare rows: references between tables, variant scoping, a key defined
twice in one variant's document, and the section 4 rules that span rows. They need the typed
rows.

Running those checks on a definition whose first-stage checks failed gives wrong answers in both
directions. A reference to a row with a broken cell would be reported as unknown, although the
row exists and already has its own error; section 7 says an error is reported once, at the row
that has it. Leaving such references unchecked instead would need every check to know which
names belonged to rejected rows, for every table and key kind, and would still mis-report the
rules that count or order rows (a protocol per variant, interpolation order, overlaps).

## Decision

1. **The checks run in stages.** The first stage checks each file on its own (`read_directory`,
   `read_files`). The second (`resolve`) checks the rules that compare rows with each other or
   with other tables. The second stage runs only when the first finds no error.
2. **Each stage lists all of its errors.** Within a stage, the reporting rules of ADR-295 item 11
   and section 7 hold unchanged: every error, sorted by file, line and column, reported once at
   the row that has it.
3. **Later rule groups join as further stages.** The field rules of section 5 and the `ir.fbs`
   rules of section 6 build on resolved references. When they land, they run as a later stage
   under the same rule, and section 7 says so.

## Consequences

- A definition with first-stage errors lists only those. After fixing them, an author can see a
  second round of errors. The rounds are bounded by the number of stages, and each round is
  complete.
- No check reports an error that follows only from another row's error, so every listed error
  is real.
- ADR-295 item 11 now holds per stage. Its status records the amendment.
