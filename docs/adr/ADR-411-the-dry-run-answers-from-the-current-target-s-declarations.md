# ADR-411: The Dry Run Answers from the Current Target's Declarations

**Date:** 2026-11-02
**Status:** Accepted
**Affects:** `diag-ir` (`dry_run_targets`, `dry_run`, `Script::response_on`, `Script::seed_on`, `DryRunEvent::SelectTarget`, `DryRunReport::target`, `DryRunError::DeclarationParts`, `ends_profile_scan`), design 12.1, glossary; ADR-325 items 3 and 4, ADR-328, ADR-390, ADR-397, ADR-409, ADR-410

## Context

ADR-328 answers every request from one variant and announces that the dry run takes the
declaration part of each target once the IR has targets. ADR-409 gave the IR its target list and
the host its notification; ADR-410 gave the restart declaration its indices. Left open: how the
dry run receives the parts and checks them against the program, whether a script's answers are
per target, how the host follows the notification, what the profile scan of ADR-390 does at a
`SelectTarget`, and what the report records of the target.

## Decision

1. `dry_run_targets(program, declarations, script, max_steps)` takes one `EcuVariant` per
   target, by index into `Program::targets`. After `Program::validate`, a number of parts other
   than `Program::target_count()` ends the run before it starts with
   `DryRunEnd::Host(DryRunError::DeclarationParts { expected, given })`: no events, step 0, pc 0.
   `Host` because it is the mocked vehicle access that cannot serve the program's targets, not
   a fault of the VM or the program. `dry_run(program, variant, ..)` stays as the call for a
   program on one target and is `dry_run_targets` with that one part (ADR-397), so its callers
   do not change.
2. The hashes in `Target::declarations` are trusted, not verified. The dry run receives
   `EcuVariant` views, not files, and the hash is taken over a file's bytes; verifying would
   mean taking bytes and adding a hashing dependency to `diag-ir`, for a hash that has no
   production writer yet. Matching the parts to the hashes is the caller's, as the job-level
   check of ADR-325 item 7 does for the agent.
3. The host keeps the current target, 0 at the start, and sets it in `select_target`. It
   answers, resolves, builds and decodes through that target's part, and the VM is stepped with
   the same part, so `DecodeField` and `FlashBlock` agree with the host.
4. What models an ECU's answer is scripted per target: `Script::response` and `Script::seed`
   address target 0, `Script::response_on` and `Script::seed_on` another. Each target has its
   own list and its own count of answers used. Keys, form and record answers and runtime inputs
   stay shared: they come from the server, the operator and the agent, and a `RuntimeInput`
   names no target (ADR-410 item 1). `Script::built_defaults` is unchanged. An answer for a
   target the program does not list is never used and is no error.
5. Every `SelectTarget` ends the profile scan of ADR-390. The scan is static and does not
   follow jumps, so it cannot tell whether a given `SelectTarget` changes the target on the path
   taken. The lowering emits one before the whole expansion of a handle's `security_access`
   (ADR-325 item 3), which the backward scan never reaches because the seed request stops it
   first; one between the steps comes only from a hand-built program, whose requests stay
   unmarked and end the run with `Undeclared`.
6. The report records the target as additions, widening no variant: `DryRunEvent::SelectTarget
   { target }` at each change of target, and `DryRunReport::target`, the target current at the
   end (0 when the run was refused before it started). The target of an event is the last
   `SelectTarget` before it, or 0; an error at `report.pc` is named with `report.target`. The
   notification before the first instruction names target 0 and records nothing, so a run on
   one target has the report it had. Over the report's budget the event is dropped and the run
   ends after the step, as for `log`; the target is updated all the same.

## Consequences

- Single-target callers and their reports are unchanged, apart from the new `target` field.
- A program with several targets can be dry-run once its caller holds every part: the server's
  ingestion, and the frontend once it emits targets. The frontend still passes one part for a
  program with no target list, and names an `Undecodable` service through that one variant;
  with several targets it must use the part of `report.target`.
- The dry run cannot catch a part that matches the count but not the hash.
- The agent has no dry run; its per-target decode check of the job's documents is ADR-325 item 7.

Rejected: an empty slice meaning the primary (there is nothing to answer from); a lazy refusal
at the first exchange on a target without a part (a validation tool should refuse up front,
and a taken path that never selects the target would hide the mistake); verifying hashes (views,
not files; a new dependency; no writer); response lists shared by all targets (one name on two
targets can have two encodings); widening `DryRunError` with a target (it changes existing
variants); a target field on `Request` and the other ECU events (it widens four variants and
writes the target in four places, where the change event writes it once); ending the scan only
at a `SelectTarget` that differs from the one before it in code order (a guess at the dynamic
target, wrong across jumps).
