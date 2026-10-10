# ADR-272: Every Journaled Run Records the State It Starts From

**Date:** 2026-10-10
**Status:** Accepted (item 2's open point for runs not from instruction 0 settled for the replay of step 4 by ADR-273; item 3 amended by ADR-274: the entry state is the newest state standing at the entry, which is the newest state or, when the newest is the current attempt's end-state record and the interruption point is that step, the one before it; the replay commits a run start since ADR-274)
**Affects:** `agent` (`src/journal.rs`, `src/journaling.rs`, `src/runner.rs`, `src/restart.rs`, `docs/ngr-agent.md`), ADR-244, ADR-252 item 6, ADR-253 item 3, ADR-255 item 3

## Context

ADR-252 item 6 records no VM state for a run that starts at a plan's entry: no step brought it
there, and its state is the program's initial one. ADR-255 item 3 starts a run again on an
existing journal when no transfer was recorded, from instruction 0. The journal marks no run
boundary. For a plan whose entry is instruction 0, the newest state in the journal can therefore
be one an earlier run took on a jump back to the entry, with that run's locals and stack. If that
run crashed before the erase and a plain start, interrupted in the transfer, follows it, the
restart's entry state (`restart::entry_state`) is the earlier run's state, and the replay of
ADR-229 item 2 step 4 and ADR-245 item 4 would start from it. ADR-255's consequences list this
and leave it open. The journal needs to say where a run began.

## Decision

1. **A new record, `RunStart { at, vm_state }`.** It is appended after
   `IntendedSoftwareVersion` in the record enum, so no format version changes (as for ADR-253,
   ADR-261 and ADR-268). On commit and on read-back it is folded like a step that carries a
   state: the journal's newest VM state becomes this one, with `at` as the step it stands at.
   It changes no `RecoveryFacts` field, so the handover summary is unchanged. It must come after
   the last step, by the same ordering check the step and marker records use: a run start that
   does not is refused on commit and makes the journal corrupt on read-back. It is limited to
   the frame size like a step's state (`TooLarge`). `Journal::commit_run_start` writes it.
2. **Every journaled run of the program from its start commits it first.** `run_on_from` commits, right after it creates the
   VM and sets the run's first step count, the run start `(pc 0, steps = first step)` with the
   encoded state, before the run's first arrival at an instruction and so before anything is
   sent. A first run's records are the creation prefix (ADR-261, ADR-268) and then the run start;
   the prefix rule allows that, since it constrains only the VIN and the version records. A
   commit that fails ends the job, as other journal failures do (ADR-252 item 3). This decision
   covers only runs that start at instruction 0. A run that starts from a restored state
   elsewhere (the replay of step 4 from a plan's entry, the continuation after a plan's end of
   ADR-271 item 3) is left to the decision that adds it, which says whether it commits a run
   start and which boundary such a state counts for.
3. **The restart's entry state is the newest journaled state.** `restart::entry_state` takes the
   newest state of the journal, run starts included. It must stand at the plan's entry (else
   `MissingEntryState`) and pass `Vm::check_state` (else `InvalidEntryState`, which also covers
   bytes that do not decode). The fallback to the program's initial state stays only for a
   journal that holds no state, which is one written before this record existed.
4. **A run start names no request.** `interruption_point` and its effective point ignore it. The
   next run's first step count counts it (`next_steps` already counts the state's step), so a run
   that crashed right after its run start still moves the count on.
5. **`RecoveryFacts` and the handover summary do not change.**

## Alternatives rejected

- **The initial state as a `Step` record.** A step claims a completed instruction, and its count
  would collide with the real first step's.
- **A classify-only rule.** The journal marks no run boundary, and inferring one from the
  repeated identity reads would couple two unrelated rules.
- **A run start only on an existing journal.** The `Vm::new` fallback would stay live for every
  new journal, so the rule that needs it would keep being exercised.
- **A format version bump.** It would make every existing journal unsupported.

## Consequences

- Each run costs one more record and sync, before its first request.
- ADR-255's consequence of a stale entry state is closed. ADR-252 item 6 is superseded. ADR-253
  item 3's "newest state" and ADR-255 item 3 are amended: a plain start journals its start, and
  the newest state includes it. ADR-244's record set is extended by one record.
- An older build reads a journal with this record as corrupt and ends in on-site intervention,
  with nothing sent. A journal written before the record keeps the old rule (the initial state
  for an entry at instruction 0, when it holds no state) until the next run on a new build adds
  a run start. A job whose stale state came from a plain start made by an older build that
  already reached the transfer keeps the old behaviour: nothing in its journal can correct it.
- A run start stands at instruction 0. For ADR-271 item 2's read-back, which keeps states apart
  by the boundary they resume at, it is therefore a candidate only for a plan whose entry is
  instruction 0.
