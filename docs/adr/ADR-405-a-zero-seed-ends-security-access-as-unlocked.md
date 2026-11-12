# ADR-405: A Zero Seed Ends `diag.security_access` as Unlocked

**Date:** 2026-11-01
**Status:** Accepted
**Affects:** `diag-frontend` (`src/profile.rs`, `src/star/lower.rs`), `diag-ir` (`is_standard_seed_level`,
`dry_run.rs`), `agent` (`comm_log/redaction.rs`), `crates/diag-frontend/docs/starlark-subset.md` 3.2,
3.4 and 6, `crates/diag-frontend/docs/csv-tables.md` 5.3; design 8.10; extends ADR-314 items 4 and 5,
amends ADR-390 item 3

## Context

ISO 14229-1 (2026) 9.4.1 has a server answer a seed request for a level that is already unlocked
with a seed of zero, never for a locked level, and has the client read a non-zero seed as
"locked". The UDS profile (ADR-314) had no rule for it: the lowering asked the host for a key and
sent it, and the procedure saw whatever the ECU answered to an unexpected key (a sequence error,
or a counted false attempt that can start the server's delay timer). The communication log already
records whether a seed is all zero and treats an empty seed as not zero (ADR-347). ADR-309 item 2
places the rules a sequence needs in the profile, and ADR-314 item 5 has `Profile::validate` check
them. 9.4.3.1 makes the seed bytes conditional on the sub-function, so a response without seed
bytes carries no value that could be zero.

## Decision

1. **The seed result map carries what a zero seed means.** `ResultMap::Seed { after_bytes, zero }`
   with `ZeroSeed::Unlocked` or `ZeroSeed::Key`. The UDS profile says `Unlocked`, and
   `Profile::validate` refuses a UDS profile that says otherwise (`ProfileError::ZeroSeed`), as it
   pins UDS routines to `RoutineControl`. Another application layer's profile chooses its own.
2. **Under `Unlocked`, an all-zero seed ends the call, on the standard seed levels.** After the positive-response check, a
   seed with at least one byte, all of them zero, skips the key step and the send-key request; the
   call's value is the seed response (`ok` true, `nrc` 0, `raw` the seed response). A procedure
   tells it from a fresh unlock by `raw`'s sub-function byte (the seed level, odd, against the key
   level, even; check `ok` first, since a negative response can match too). A non-zero byte
   anywhere sends the key as before. The rule, and the empty-seed refusal of item 3, apply only
   on the levels where ISO 14229-1 (2026) 9.4.2.2 pairs requestSeed with sendKey (odd from 0x01
   to 0x41, and 0x5F), the same set for which the communication log records `all_zero`
   (ADR-347). On the supplier-specific levels (0x61 to 0x7D), for which the standard defines no
   pairing, the key step runs as before whatever the seed. One definition,
   `diag_ir::is_standard_seed_level`, serves the lowering and the agent's redaction.
3. **A seed response without seed bytes ends the procedure.** Such a response, or one shorter
   than the bytes before the seed, is neither a zero seed nor a seed the key can be computed from:
   the procedure ends with `Fail` code `SEED_EMPTY` and a detail naming the line (ADR-280),
   as a declared failure and not a new `VmError` variant, which would change the IR (item 4). The
   key step is not run.
4. **The check is built from existing instructions.** A byte loop over `Length` and `IndexGet`,
   sharing its counted-loop skeleton with the flash block loop (`Lowerer::counted_loop`, ADR-397),
   so the instruction set, `VmError` and `IR_SCHEMA_VERSION` do not change; the IR golden files
   do. The loop reaches no host, so the dry run's marking of the profile's requests by place
   (ADR-390) holds; the `Fail` is placed out of line after the send-key request because a `Fail`
   between the seed request and the `SecurityAccess` would end that scan.
5. **The dry run's seed request never gets a zero seed (amends ADR-390 item 3).** The request
   ADR-390 marks as the seed request is answered, when no script answer exists, with the
   positive SID, the level and `BUILT_SEED` (1, 2, 3, 4), cut or repeated to the declared seed
   field's fixed length when the variant declares the service and the field has one. A declared
   service's answer built from its declaration alone would be `67` with no seed, or zeros, and
   either one ends the built run at once (an empty seed fails, a zero seed skips the key step),
   hiding the rest of the procedure from the conversion's check (ADR-391). One function builds
   the seed response for the declared and the undeclared service alike.
6. **ADR-314 items 4 and 5 are extended.** Item 4: `raw` is the key response, or the seed
   response when the level was found unlocked. Item 5: `validate` also pins `zero` on UDS.

## Consequences

- A procedure that unlocks twice, or an access program rerun after a link reopening (ADR-325),
  no longer sends a key to an unlocked ECU. The access program case needs targets in the IR,
  which are separate work; it inherits the rule when they exist.
- 20 VM steps per seed byte plus about 15 fixed; a 16-byte seed costs about 335 of the step
  budget. Each byte test copies the seed (`LoadLocal`), so the cost in time is quadratic in the
  seed's length, and an all-zero seed above about 50 kB reaches the step limit; no real seed is
  near that.
- At the call's end the response slot and the seed slot are overwritten with 0, and the stack
  is empty. The loop index is a counter and is left. The `Fail` path of item 3 clears nothing,
  as a failure leaves the state in place, but holds no seed bytes: the seed is empty and the
  response slot holds only the bytes before it.
- An ECU or simulator that answers a level it has not unlocked with a seed of zeros is read as
  unlocked; 9.4.1 forbids that answer.
- A response echoing another level is still not checked by the lowering (ADR-400 governs matching
  of answers to requests).
- The agent, the journal, the communication log and `sim-ecu` are unchanged.
