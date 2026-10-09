# ADR-252: The Job Runner Journals at the Flash Recovery Plan's Boundaries

**Date:** 2026-10-08
**Status:** Accepted (item 2's open point decided by ADR-253)
**Affects:** `agent` (`src/runner.rs`, `src/journaling.rs`, `src/inputs.rs`, `src/host.rs`), `j2534-0404-service` (`tests/agent_flash_transfer.rs`)

## Context

ADR-244 item 7 leaves to the job runner the commits that guard requests: the transfer-start
marker before the first erase or RequestDownload, the RequestTransferExit marker before that
request, and ending the job when a commit fails. A restart (ADR-229 item 2) also reads the ECU
hardware part number and the pre-erase software version from the journal, and replays the
procedure from the plan's entry boundary, which needs the VM state there (ADR-245 item 4).
The runner kept no journal, so none of this was written. Several choices are not settled by
those ADRs: when the identity is read, how a reading becomes the bytes the journal stores,
what happens when it cannot be read, and how the VM state at the entry is recorded when there
is no step before it.

## Decision

1. **Markers are committed when execution arrives at a boundary, before its instruction
   runs.** For each plan of the program:
   - at `entry_pc`, once per job and only before the job's first transfer: the identity
     (item 3);
   - at `erase_pc`: the transfer-start marker, with the plan's stage;
   - at `transfer_exit_pc`: the RequestTransferExit marker;
   - before the first primitive at or past `FromPc`: the intent that ADR-253 adds.

   An arrival is counted once: a timer wait that polls the same instruction does not commit
   again. Before any of this, `Vm::current_op` runs the VM's own checks of the instruction
   (ADR-233 item 3), so an instruction the VM refuses before it reaches the host fails with no
   marker committed for it.
2. **Progress is committed when an instruction completes.**
   - A `FlashTransfer` commits its block under the host's running index (ADR-250).
   - Every diagnostic primitive inside a plan's range (`entry_pc` up to, not including,
     `post_transfer_end_pc`) commits a step record, so the journal's last step orders the
     interruption point (ADR-229 item 1, ADR-245 item 4) and records the post-transfer
     progress (ADR-244 item 3). A recovery-required point can lie anywhere in the plan, after
     the RequestDownload or among the post-transfer steps, so a primitive that completed at or
     after it is always on record. A block therefore costs two records: the block and its step.
     Only the erase and RequestTransferExit are written ahead of their requests: a request sent
     whose response was lost leaves the last step on the primitive before it, so the
     interruption point of ADR-229 item 1 can fall before a recovery-required point that a
     later request crossed. ADR-253 closes this: it adds a third write-ahead record, an intent
     before the first primitive at or past the recovery-required point, and decides how a
     restart reads it.
   - A step that brings execution to a plan's `entry_pc` commits its step record with the VM
     state after it (ADR-244 item 6), which is the state the restart replays from.
   - A step that brings execution to a plan's `post_transfer_end_pc`, after its exit marker,
     commits the post-transfer completion right after its step record. This is part of the
     step, so a job stopped right there (a cancel, the step limit) still records it, and a
     plan's end that is the next plan's entry completes the one before entering the next.
3. **A failure stops before the next instruction.** A commit that fails, or an identity that
   cannot be read, ends the job before the next instruction runs, so a guarded request is never
   sent without its marker, and nothing is erased without the identity. A cancelled job (ADR-235: the call in flight finishes, no further instruction runs) stops
   before each identity read and again before the boundary's instruction.
4. **The identity is read at the plan's entry, once per job, before the first transfer.** The
   hardware part number and the software version are read through the sources the program
   declares, at the entry rather than at the erase. The entry is where a restart's replay
   starts, typically in the default session, where an ECU answers identification reads that it
   may refuse in a programming session. They are read once: a loop back to the entry before
   the erase (which ADR-245 item 4 allows) does not read them again, since by then the ECU may
   be in its programming session. The journal takes the pre-erase version only before the
   job's first transfer (ADR-244 item 4).
   - A declared source that gives no value ends the job before the erase
     (`JobError::IdentityUnreadable`), and a failure to send the read is
     `JobError::IdentityRead`. A restart compares the ECU against these values, and without
     them it could only end in on-site intervention.
   - The exception is a software version answered with the plan's declared "no valid
     application" response (design 8.2.5): there is no version to record, and the job goes on
     without one. A restart then treats that response as a transfer to redo (ADR-229 step 3).
   - An undeclared source is skipped (a plan that never allows a restart need not declare it).
5. **The journal stores the raw field bytes.** `inputs::read_field_bytes` sends the source's
   request and applies the same checks as `resolve_source` (positive response, exact echo, no
   extra bytes, a field that decodes under its encoding), then returns the field's bytes, not
   the decoded value, as `RecoveryFacts` describes them. A restart compares with the same
   function, so no decoding rule can make two different answers equal. A negative response is
   reported with its code, so the declared "no valid application" answer can be recognised.
6. **A job that starts at the entry records no state there.** No step brought it there; its
   state is the program's initial state, which the program itself gives.
7. **The journal is created once the link is open and the policy allows the program.**
   `run_program_journaled` creates the journal of the given key in the given directory, with
   the identity table, after the link opens and the program passes the link's permission, and
   before anything is sent to the ECU. A link that fails to open, or a refused program, leaves
   no journal behind, so the job can be tried again under the same key; a journal that cannot
   be created, or one that already exists, ends the job with nothing sent. A program with no
   flash recovery plan keeps no journal and creates no file. `run_program` (and so
   `ngr-agent run`) still keeps none.

## Consequences

- The integration test against `sim-ecu` writes a journal for a download of 300 blocks and
  reads it back: the last block number, both markers, the completion and the identity.
- Each block costs two syncs, its block and its step record (ADR-244 consequences allow
  batching them later without a format change).
- The identity is read once per job. A procedure that redoes its transfer inside one job (a
  jump from the post-transfer steps back to the erase) does not read it again.
- This ADR decides what a first run writes. Reading it back (opening an existing journal, the
  resume count, the replay from the entry) is the restart's part of ADR-229 item 2.
