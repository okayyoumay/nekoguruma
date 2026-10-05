---
name: next-task
description: Recommend what to work on next in nekoguruma by surveying the whole backlog in work/ (project-wide and worker-crate files). Use when the user asks what to do next, what the top priority is, or which open item to pick up. Produces one pick plus one alternative, not a full list.
argument-hint: "[area or crate to focus on]"
context: fork
agent: Explore
model: sonnet
effort: medium
background: false
---

# Next task

Give one recommendation the user can act on, with the reasoning, not an
inventory. For a plain list of open items, just read the backlog files.

This skill runs as a forked read-only subagent on Sonnet: the backlog files
are large, and only the recommendation comes back to the main conversation.
Focus area, if the user gave one: $ARGUMENTS

## Steps

1. **Read every backlog file** in `work/` (the file table in
   `work/README.md` lists them). If the user named an area or crate, focus
   on it, but still note a P0 elsewhere.
2. **Collect candidates**: all P0 items, then P1 items. Every entry under a
   `Known Flaky Tests` section is a P1 candidate too (`work/README.md`).
   Go to P2 only if
   there are no P0/P1 items in scope. Skip items with a `Blocked on:` clause
   unless the blocker is resolved (say so if you find one that is).
3. **Verify the top candidates are still open.** For each of the best two
   or three, check the code or docs the item cites (one or two targeted
   reads or a quick `git log` on the paths). A finished item is not a
   recommendation: report it as stale instead (the `backlog` skill closes
   it).
4. **Rank** by, in order:
   - priority;
   - whether other items depend on it (it unblocks more work);
   - whether it is on the path of the current milestone (the project-wide
     backlog's Status section);
   - size: prefer the item that fits in one pull request.
5. **Report** (about 15 lines):
   - **Pick**: the item, in a sentence, with its file and section.
   - **Why now**: the deciding reasons.
   - **First step**: where to start (paths, design section, ADR).
   - **Alternative**: one other item and when it would be the better choice.
   - **Stale items found**: any item that turned out already done.

Do not edit the backlog in this skill; recommend only. If the user then
asks to start the pick, work on it normally.
