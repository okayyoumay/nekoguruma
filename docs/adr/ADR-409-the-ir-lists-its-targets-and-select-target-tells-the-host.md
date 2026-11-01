# ADR-409: The IR Lists Its Targets and `SelectTarget` Tells the Host

**Date:** 2026-11-01
**Status:** Accepted
**Affects:** `diag-ir` (`Program::targets`, `Target`, `Op::SelectTarget`, `VmState::target`, `DiagHost::select_target`, `VmError::BadTarget`, `Program::validate`, `IR_SCHEMA_VERSION` 21), `agent` (`check_runnable`, `WorkerHost`), design 8.2.4; ADR-233 items 3, 9 and 10, ADR-325 item 3, ADR-397

## Context

ADR-325 item 3 decides that a `Program` lists its targets, that a `SelectTarget` instruction
makes one current, and that the host is told on every change and when a run starts from a
restored state. It leaves the IR's shapes open: how a program that declares no target is
represented while the frontend emits none, how `SelectTarget` names a target and what an index
outside the list does, whether the notification can fail, when a restored state's target
reaches the host, and what an agent that opens one link per job does with a program that lists
several.

## Decision

1. `Program::targets` is a list of `Target { access, declarations }` in declaration order, the
   primary first; `declarations` is the content hash (`plan::Sha256`) of the declaration-only
   document the program was lowered against for that target. An empty list means the primary
   alone, permanently, so hand-built programs, the identification program and existing tests
   need no placeholder hash. `Program::target_count()` is the one definition of how many
   targets there are: the list's length, or one for an empty list.
2. `Op::SelectTarget { target: u8 }` indexes the list, appended after `HmiRequestFromStack`
   (ADR-233 item 10). An index at or past `target_count()` is refused statically by
   `Program::validate` (`TargetOutOfRange`), at run time by the VM before any host call
   (`VmError::BadTarget`), and in a restored state by `Vm::check_state` (ADR-233 item 9 gains
   the check), so a damaged journal cannot address a target the program does not list.
   `VmState::target` is a `u8` too; the width suits the limit of eight targets (`MAX_TARGETS`).
3. The notification `DiagHost::select_target` is infallible and has a default empty body. The
   VM makes it once before the first instruction of every run, new or restored, after the
   state and the instruction have passed their checks (so a refused state never reaches the
   host), and on every `SelectTarget` that changes the target; selecting the current target
   makes no call. A finished state runs no instruction and tells nothing. A host that cannot
   serve the target refuses its next exchange.
4. `SelectTarget` is not a diagnostic primitive: it sends nothing, so nothing is journaled
   around it and no intent marker precedes it.
5. `Program::validate` bounds the list to eight and refuses two targets with the same
   declaration hash, the IR's image of ADR-325 item 1's rule against two roles bound to one
   definition.
6. Until one link per target exists (ADR-325 item 8), the agent's `check_program` and
   `check_job` refuse a program that lists more than one target before any worker starts
   (`JobError::SeveralTargets`), and `WorkerHost` refuses an exchange made after it was told
   a target other than the primary (`HostError::Unsupported`). The second is defence in depth
   for a caller that steps the host without the checks.

## Consequences

`IR_SCHEMA_VERSION` is 21. A version-20 program, document or journal state is refused with no
migration (design 8.2.6); a journal state of version 20 does not decode and ends in on-site
intervention, the existing path for an undecodable state. The golden files of `diag-frontend`
and the fuzz seeds are regenerated. `ir.fbs` does not change: the target list is procedure-part
data in the postcard `Program`, like the restart declaration (ADR-245).

The hash has no production writer yet (the frontend emits an empty list) and no production
reader (the job-level check of ADR-325 item 7); it is carried now so the later parts do not
bump the version again. Part 2 adds the restart declaration's target names and the rules that
depend on them; part 3 gives the dry run one declaration part per target.

Rejected: a fallible notification (no planned host can fail there, and it would make a
non-primitive instruction a run-ending host error); telling the host only from `Vm::resume` (a
fresh run would rely on the host's assumed default); requiring a non-empty list now (it needs a
made-up hash); a `u32` operand (an index into a list of at most eight).
