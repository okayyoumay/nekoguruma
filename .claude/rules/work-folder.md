---
paths:
  - "work/**"
---

# work/ (temporary material)

- Every file here starts with the "TEMPORARY WORKING MATERIAL" line used by
  the existing files.
- Backlog items follow `work/README.md` ("Backlog format"): one top-level
  bullet per item starting with `- **P0**` to `- **P3**`, self-contained
  (cite paths, design sections and ADRs; never "see above"), with a "Done
  when", sorted by priority within the section. P0 only for real harm or
  work that stops everything; P1 means needed for the current milestone
  (the project-wide backlog's Status section); waiting items get a
  `Blocked on:` clause rather than a lower priority. Entries under `Known Flaky
  Tests` count as P1. Use the `backlog` skill to add or close items.
- A finished item is deleted, not checked off, struck through or moved to a
  "resolved" list. Before deleting, move any lasting decision into the
  permanent document it belongs to, and keep any still-open remainder as its
  own item.
- Delete a section or file once it has no items left, and keep the file
  table in `work/README.md` current.
- Never cite these files from outside `work/`.
- Run `scripts/check-backlog.sh` after editing a backlog file.
