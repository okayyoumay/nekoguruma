# ADR-252: The Job Runner Journals at the Flash Recovery Plan's Boundaries

**Date:** 2026-10-08
**Status:** Accepted
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

1. **Commits are tied to the plan's boundaries, made when execution arrives at an
   instruction, before it runs.** For each plan of the program:
   - at `post_transfer_end_pc`, when the job's transfer of the plan's stage has its exit marker
     and is not complete: the post-transfer completion. This is checked first, so a plan's end
     that is the next plan's entry completes the one before entering the next;
   - at `entry_pc`: the VM state, then the identity (items 3 and 4);
   - at `erase_pc`: the transfer-start marker, with the plan's stage;
   - at `transfer_exit_pc`: the RequestTransferExit marker.

   After a `FlashTransfer` completes, the block is committed under the host's running index
   (ADR-250). An arrival is counted once: a timer wait that polls the same instruction does not
   commit again.
2. **A failure stops before the boundary's instruction.** A commit that fails, or an identity
   that cannot be read, ends the job before the instruction at that boundary runs, so the
   request a marker guards is never sent without it. A failed block commit ends the job before
   the next instruction.
3. **The identity is read at the plan's entry, before the job's first transfer.** The
   hardware part number and the software version are read through the sources the program
   declares, at the entry rather than at the erase: the entry is where a restart's replay
   starts, typically in the default session, where an ECU answers identification reads that it
   may refuse in a programming session. They are read only while the job's journal holds no
   transfer, since the journal takes the pre-erase version only before the first transfer
   (ADR-244 item 4). A source the program declares that gives no value ends the job before the
   erase (`JobError::IdentityUnreadable`): a restart compares the ECU against these values, and
   without them it could only end in on-site intervention. An undeclared source is skipped (a
   plan that never allows a restart need not declare it).
4. **The journal stores the raw field bytes.** `inputs::read_field_bytes` sends the source's
   request and applies the same checks as `resolve_source` (positive response, exact echo, no
   extra bytes, a field that decodes under its encoding), then returns the field's bytes, not
   the decoded value, as `RecoveryFacts` describes them. A restart compares with the same
   function, so no decoding rule can make two different answers equal.
5. **The VM state at the entry is the state after the step that brought execution there.** It
   is committed on that step's record (ADR-244 item 6). A job that starts at the entry has no
   such step; its state is the program's initial state, which the program itself gives, and no
   record is written.
6. **The journal is created before the link opens.** `run_program_journaled` creates the
   journal of the given key in the given directory, with the identity table, before anything
   is sent; a journal that cannot be created, or one that already exists, ends the job with
   nothing sent. A program with no flash recovery plan keeps no journal and creates no file.
   `run_program` (and so `ngr-agent run`) still keeps none.

## Consequences

- The integration test against `sim-ecu` writes a journal for a download of 300 blocks and
  reads it back with every block, both markers, the completion and the identity.
- Each block costs a sync (ADR-244 consequences); journaling 300 blocks added about a second
  and a half to the integration test on a development machine.
- The identity is read once per job. A procedure that redoes its transfer inside one job (a
  jump from the post-transfer steps back to the erase) does not read it again.
- This ADR decides what a first run writes. Reading it back (opening an existing journal, the
  resume count, the replay from the entry) is the restart's part of ADR-229 item 2.
