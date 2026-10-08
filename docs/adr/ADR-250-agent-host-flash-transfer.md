# ADR-250: The Agent Host Implements FlashTransfer with Its Own Block Count

**Date:** 2026-10-08
**Status:** Accepted
**Affects:** `agent` (`src/host.rs`, `src/policy.rs`), `diag-ir` (`src/recovery.rs`), `j2534-0404-service` (`tests/agent_flash_transfer.rs`), ADR-245 items 4 and 6, ADR-247 Decision item 4

## Context

`WorkerHost` returned "unsupported" for the `FlashTransfer` instruction, and the policy refused
it (ADR-247 Decision item 4), so no procedure could transfer data (design 8.2.5). The data
transfer part of an interrupted-write job needs it.

The instruction `Op::FlashTransfer { block }` has an operand that is constant for the
instruction. The TransferData block sequence counter on the wire starts at 1 after each
RequestDownload (or RequestUpload), rises by one per request, and after 0xFF continues at 0x00
(ISO 14229-1:2026 clause 14.4). A loop over blocks therefore cannot take the counter from the
operand, and a transfer of more than 255 blocks wraps it (ADR-229 context). Changing the
instruction would be an IR schema change (ADR-245 item 7).

## Decision

1. **The host implements `FlashTransfer`.** It sends TransferData (0x36: counter, then the data
   from the VM) through the same send-receive path as every other request, so the policy check
   and the response matching are unchanged. The response must be positive and echo the counter.
   A negative response, another service's answer or a wrong echo is a host error (the job
   fails; nothing is assumed accepted).
2. **The wire counter comes from the host's running count.** The host counts the blocks the ECU
   confirmed since the last RequestDownload. The next block has index `count + 1` and the wire
   counter `index mod 256`: 1 for the first block, 0xFF for the 255th, 0x00 for the 256th, then
   0x01 again. Only a positive response that echoes the counter counts the block. Anything
   else ends the tracked transfer: a negative response, a positive one that does not echo the
   counter, and no response at all (timeout, lost events, transport failure). In each case
   whether the ECU kept the data is unknown (a missing response may hide a negative one), and
   clause 14.4 lets the ECU acknowledge a repeat of the previous counter without writing it,
   so a same-counter retry could leave a gap. A new RequestDownload is needed, which is what
   the restart order of ADR-229 does.
3. **What begins and ends the tracked transfer.** Only a `ServiceRequest` with service 0x34
   answered positively begins it, at count 0; a second one mid-transfer starts over. These end
   it whatever the response: RequestUpload (0x35, which never begins one, since `FlashTransfer`
   would discard upload data), RequestTransferExit (0x37), RequestFileTransfer (0x38),
   DiagnosticSessionControl (0x10) and ECUReset (0x11); so does a failed or unanswered 0x34. A
   `FlashTransfer` with no tracked transfer is an error and sends nothing, so a resumed job
   cannot send blocks with a guessed counter. A `ServiceRequest` for TransferData (0x36) is
   refused by the host, so every block goes through the count.
4. **The IR operand `block` is ignored by the host.** It is constant per instruction and carries
   no information the running count lacks. The IR is unchanged.
5. **The journal records the running index.** `WorkerHost::transfer_block_index` gives the
   number of blocks the ECU confirmed, which rises through the transfer without wrapping at
   255. Any failed block ends the transfer, so the journal records each block as it is
   confirmed; after a failure the ECU may hold one block more than the last one recorded.
   `Some(0)` means a transfer started with no confirmed block, not a block number. The
   write-job journal (ADR-244) records this index, not the operand and not the wire counter.
   It is a `u64` while `Journal::commit_block` takes a `u32`, so the journaling runner
   converts it and never commits 0.
6. **Policy.** The simulator permission of ADR-247 covers the `FlashTransfer` instruction, as it
   covers `RoutineControl`, and the read-only permission refuses it (service 0x36). This amends
   ADR-247 Decision item 4: only the refusal of `FlashTransfer` is superseded. The
   `SecurityAccess` instruction stays refused, because the host still has no implementation.
7. **The validator keeps a plan's transfer whole.** Each of DiagnosticSessionControl (0x10),
   ECUReset (0x11), RequestUpload (0x35) and RequestFileTransfer (0x38) ends the host's
   tracked transfer (item 3), and a second RequestDownload starts it over without a new
   erase, so `Program::validate` keeps them out of a running transfer:
   - it refuses any of the four between a plan's RequestDownload and its RequestTransferExit
     (`ProgramError::TransferInterrupted`);
   - a jump inside the transfer may not go back to the RequestDownload or before it
     (`ProgramError::JumpOutOfRecovery`). Before, ADR-245 item 4 let it go back as far as the
     erase, so a loop could re-run a RequestDownload, or one of the four requests placed
     between a routine-control erase and the RequestDownload, mid-transfer.

   Before the RequestDownload the four requests stay allowed, since execution cannot return
   there once the transfer has begun. This amends ADR-245 items 4 and 6.

## Consequences

- A debug agent against `sim-vci` can run a complete download: erase, RequestDownload,
  TransferData blocks, RequestTransferExit. A release agent still cannot transfer.
- A transfer longer than 255 blocks works; the integration test transfers more blocks than the
  counter holds before it wraps.
- RequestFileTransfer (0x38) also resets the server's counter, but the host does not begin a
  transfer on it; it only ends the tracked one.
- The host does not repeat a block, and a failed block ends the tracked transfer; the job
  ends on the host error, and the restart rules of ADR-229 redo the transfer from
  RequestDownload.
- A `ServiceRequest` for TransferData (0x36) is refused before the link opens
  (`policy::check_program`), not only by the host, so a program carrying one fails before
  anything is erased.
- A program that could run DiagnosticSessionControl, ECUReset, RequestUpload,
  RequestFileTransfer or a second RequestDownload while its transfer runs is refused when it
  loads (item 7), before anything is erased. Download requests appear only inside a plan
  (ADR-245 item 6), so a valid program reaches the host's end rules of item 3 only through
  the plan's own RequestTransferExit.
- A procedure cannot retry the transfer by looping back to its RequestDownload; a retry is a
  restart from the erase (ADR-229).
