# ADR-250: The Agent Host Implements FlashTransfer with Its Own Block Count

**Date:** 2026-10-08
**Status:** Accepted
**Affects:** `agent` (`src/host.rs`, `src/policy.rs`), `j2534-0404-service` (`tests/agent_flash_transfer.rs`), ADR-247 Decision item 4

## Context

`WorkerHost` returned "unsupported" for the `FlashTransfer` instruction, and the policy refused
it (ADR-247 Decision item 4), so no procedure could transfer data (design 8.2.5). The data
transfer part of an interrupted-write job needs it.

The instruction `Op::FlashTransfer { block }` has an operand that is constant for the
instruction. The TransferData block sequence counter on the wire starts at 1 after each
RequestDownload or RequestUpload, rises by one per request, and after 0xFF continues at 0x00
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
   accepted since the last RequestDownload or RequestUpload. The next block has index
   `count + 1` and the wire counter `index mod 256`: 1 for the first block, 0xFF for the 255th,
   0x00 for the 256th, then 0x01 again. A block that fails is not counted, so a retry repeats
   the same counter, which the standard obliges the server to accept.
3. **The count resets on a download or upload request.** A `ServiceRequest` with service 0x34
   or 0x35 sent through the host ends the earlier transfer, and a positive response starts a
   new one at count 0. A `FlashTransfer` with no transfer started on this host is an error and
   sends nothing, so a resumed job cannot send blocks with a guessed counter: it restarts from
   RequestDownload (ADR-229).
4. **The IR operand `block` is ignored by the host.** It is constant per instruction and carries
   no information the running count lacks. The IR is unchanged.
5. **The journal records the running index.** `WorkerHost::transfer_block_index` gives the
   index of the last accepted block, which rises monotonically through the transfer without
   wrapping at 255. The write-job journal (ADR-244) records that, not the operand and not the
   wire counter.
6. **Policy.** The simulator permission of ADR-247 covers the `FlashTransfer` instruction, as it
   covers `RoutineControl`, and the read-only permission refuses it (service 0x36). This amends
   ADR-247 Decision item 4: only the refusal of `FlashTransfer` is superseded. The
   `SecurityAccess` instruction stays refused, because the host still has no implementation.

## Consequences

- A debug agent against `sim-vci` can run a complete download: erase, RequestDownload,
  TransferData blocks, RequestTransferExit. A release agent still cannot transfer.
- A transfer longer than 255 blocks works; the integration test transfers more blocks than the
  counter holds before it wraps.
- RequestFileTransfer (0x38) also resets the server's counter, but the host does not track it
  as a transfer start: it is not used by any procedure.
- A repeated block after a lost response is not repeated by the host itself; the VM's host
  error ends the job and the restart rules of ADR-229 apply.
