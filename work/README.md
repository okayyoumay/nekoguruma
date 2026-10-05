> **TEMPORARY WORKING MATERIAL.** Everything under `work/` is consumable: task lists, open items and status notes that are worked through and then deleted. It is not part of the design or the product.

# work/

Rules for this folder:

- Files here describe the current state of the work (what is done, what is next, what is still unverified). Delete or rewrite an entry once it is done.
- Permanent files (`README.md`, `docs/`, `api/`, `db/`, `schemas/`, `crates/`, `scripts/`, `.github/`) must not reference anything in `work/`, and must not depend on its numbering (e.g. "step 3", "task 2").
- When an item here produces a lasting decision, write that decision into the permanent file it belongs to (usually `docs/system-architecture.md` or a crate/folder README), then remove the item from here.
- Every file in this folder starts with the same "TEMPORARY WORKING MATERIAL" line.

## Files

| File | Contents |
|---|---|
| `backlog.md` | Project-wide status and open items, per area |
| `worker-crates-backlog.md` | Open items and known flaky tests of the worker crates (`iso22900*`, `j2534-0404*`, `vci-service-*`), per crate |
| `development-plan.md` | Milestones with goals and exit criteria; says which milestone is current |

Backlog files are named `*backlog.md`; only they are checked against the backlog format below.

## Backlog format

A backlog file has `##` sections per area (a crate, a folder or a topic). Worker-crate sections have `### Prioritized Backlog` and `### Known Flaky Tests` subsections.

Each open item is one top-level bullet that starts with its priority:

```markdown
- **P1**: Short statement of what is missing. Where it is (`path`, design section, ADR). Done when: what a reviewer or a test can check.
```

- **Self-contained.** An item names the files, design sections and ADRs it is about. Never write "see the entry above"; items are added and deleted independently.
- **Done when.** New items say how to tell they are finished. Older items may lack it; add it when you touch the item.
- **Order.** Within a section, keep items sorted by priority (P0 first).
- **One item, one piece of work.** Split an entry that holds several independent tasks.
- **No history.** A finished item is deleted, not checked off, struck through or moved to a "resolved" list. Before deleting, write any lasting decision into the permanent document it belongs to, and keep any still-open remainder as a new item.

Priorities:

| Priority | Meaning | Examples |
|---|---|---|
| **P0** | Can cause real harm, or stops all other work. Fixed before anything else starts. | Can brick an ECU or leave a vehicle in an unsafe state; loses or corrupts write-job or journal state (design 5.5 / 5.6); breaks a trust boundary (package signatures, worker auth tokens, journal encryption; design 5.5 / 11); main is red |
| **P1** | Needed for the current milestone (named in the project-wide backlog's Status section). | A missing piece on the milestone's path; a correctness bug the milestone exercises |
| **P2** | Should be done, but the current milestone does not need it. | Test gaps outside the milestone path; docs; dev-only dependency advisories |
| **P3** | Nice to have. | Diagnostics, refactors, polish |

- P0 is rare. A backlog with many P0 items has no P0; re-check them against the examples.
- Priority is how much the item matters once it can be done, not whether it can be done now. An item that waits on a decision, a vendor or another item states that with a `Blocked on:` clause (e.g. `Blocked on: design 17 P7.`) instead of being lowered to P3; `next-task` skips blocked items. The clause names what unblocks the item: a standard, data or hardware to obtain, another item, or a decision. A design choice that the available specs and ADRs leave open (several readings are defensible) is Yoko's to make and is written as `Blocked on: Yoko's decision on ...`; such items are collected for her rather than picked up, and a `design-advisor` analysis can be attached to help her decide. A question the specs or existing ADRs already answer is part of the item's work, not a blocker.
- When the milestone changes, re-check the P1 and P2 items against it.
- Every entry under a `Known Flaky Tests` section counts as **P1**, whatever the milestone (decided by Yoko, 2026-10-04): a flaky test makes red CI ambiguous and hides real regressions. Fixing one means finding the cause and making the test deterministic (never skipping, disabling or loosening it), then deleting the entry. `next-task` ranks these entries with the P1 items.

Sections that are not item lists are exempt from the bullet format: `Status`, `Unverified Assumptions`, `Known Flaky Tests` and anything below them. A known flaky test entry names the test, the symptom, how to confirm it in isolation, and what is known about the cause. These entries carry no priority prefix; they are all P1 (see the list above).

`scripts/check-backlog.sh` checks the header line and the priority prefix of every item; it runs in CI.
