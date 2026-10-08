---
name: backlog-triage
description: Review the whole nekoguruma backlog in work/ for stale, duplicate, mis-prioritized or badly formatted items and fix them. Run only when the user asks for a backlog cleanup or triage.
disable-model-invocation: true
argument-hint: "[file or section to limit the review to]"
---

# Backlog triage

A cleanup pass over the backlog files in `work/`, following the format in
`work/README.md`. This is a sizeable read; limit it to the file or section
the user named, if any.

## Steps

1. Run `scripts/check-backlog.sh` and fix any format errors.
2. For each item in scope, decide one of:
   - **Done**: the cited code or docs show it is finished. Close it with the
     `backlog` skill's "Close an item" steps (residuals become new items,
     lasting decisions move to permanent docs).
   - **Duplicate**: merge into the clearer item, keeping all evidence.
   - **Too big**: split into self-contained items, one piece of work each.
   - **Wrong priority**: re-prioritize against the table in
     `work/README.md` and the current milestone. Items carried over from the
     worker crates' earlier scale need this most (their P3 meant "lower
     priority", not "nice to have"). A development-process item below P1
     goes up to P1 and moves to the "Development process" section
     (`work/README.md`).
   - **Blocked**: an item lowered only because it waits on something gets
     its real priority back plus a `Blocked on:` clause; a resolved blocker
     is removed.
   - **Vague**: add "Done when" and the paths or sections it concerns.
   - **Keep**: no change.
   Check the code for "Done" with targeted reads; do not mark an item done
   on its wording alone. For a large file, hand the read-only verification
   to `code-scout` in batches, one call per section.
3. Remove history the format does not allow: "resolved" lists, struck-out
   or checked-off entries, dated change logs inside items. Move anything
   still needed (a lasting decision, a root-cause explanation for a test
   that is still flaky) to the permanent document it belongs to or into
   the remaining item.
4. Keep each section sorted by priority; delete empty sections and files and
   update the file table in `work/README.md`.
5. Run `scripts/check-backlog.sh` and `scripts/check-work-refs.sh`.

Report a table of what changed (closed, merged, split, re-prioritized), and
list items you could not verify. Open a pull request with the changes; the
user reviews the closures.
