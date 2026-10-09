# ADR-244: Write-Job Journal as an Append-Only Record Log

**Date:** 2026-10-07
**Status:** Accepted (record set extended by ADR-253; the no-lock consequence superseded by ADR-255 item 7)
**Affects:** `crates/agent/src/journal.rs`, `crates/agent/tests/journal_crash.rs`, `docs/system-architecture.md` (5.5)

## Context

ADR-229 restarts an interrupted transfer in a fixed order that reads its inputs from the
agent's journal: the last confirmed step, the ECU hardware part number and the pre-erase
software version, a transfer-start intent marker committed before the first erase or
RequestDownload, a RequestTransferExit intent marker committed before that request, the
post-transfer progress, and the per-stage resume count with the attempt key it was committed
with. Two of its updates must be atomic: a resume count with its attempt key, and a new
transfer-start marker with the clearing of the previous attempt's exit marker and post-transfer
progress. The same facts form the checkpoint summary a handover carries (design 8.2.5), and the
journal of a failed device is uploaded for audit (ADR-229), so its history matters as well as
its latest state. A flash transfer commits once per block, thousands of times, on ext4 and NTFS,
and a commit that returned must survive a crash or a loss of the device's power.

The options were a whole-state snapshot replaced on every commit (temporary file, sync,
rename, directory sync) and an append-only log of records.

## Decision

1. **An append-only log, one file per job and ownership generation.** The file
   `{job_id}.g{generation}.journal` starts with a magic, a format version and a header frame
   naming the job and generation; every later frame is one record. A frame is its payload's
   length, a CRC-32 of the payload, and the payload (postcard). Record variants are only ever
   appended, since postcard encodes the variant index (as ADR-233 item 10 does for the IR). A
   log appends a few dozen bytes per block, where a snapshot would rewrite the whole state, VM
   state included, and it keeps the history the audit upload needs. The ownership generation
   in the name follows ADR-229: a device that gets a job back starts a new journal.
2. **One commit is one frame and one sync.** A commit appends its frame and syncs the file
   before it returns; the facts the journal reports change only after that. Appending to an
   existing file needs no rename, so no commit depends on rename durability, which `std` does
   not provide on Windows. A failed write or sync poisons the journal: it takes no more
   records, since a failed sync can drop data the OS had accepted and a later commit would then
   claim what was lost.
3. **Atomic updates are single records.** A resume record carries the new count and the attempt
   key together. A transfer-start record replaces the whole transfer attempt, so the previous
   attempt's exit marker, post-transfer progress and last block go with it. Post-transfer
   completion is its own record; steps after the exit marker count as post-transfer progress
   until it. A resume record closes the attempt it interrupted: that attempt takes no more
   blocks, exit marker, post-transfer progress or completion, since the steps that follow
   belong to the recovery, and only a new transfer-start marker opens a transfer again.
4. **The folded state is the journal's part of the summary.** Reading the journal folds its
   records into `RecoveryFacts`, which the checkpoint summary sent for handover carries
   unchanged, next to what the job itself names (target VIN and ECU, the version being written,
   the stage reached; design 8.2.5). Records that contradict the facts (a block outside a transfer, a
   second exit marker, a step that does not come after the last one, a pre-erase version after a
   transfer started, a resume count that does not count one more) are refused when committed and
   make a journal corrupt when read back. The hardware part number cannot change once a transfer
   started, since a restart compares the ECU against it, like the pre-erase version, which
   cannot be recorded after that point at all.
5. **Torn tail versus corruption.** Only what one unfinished commit can leave is cut off when
   the journal is opened: a last frame that runs past the end of the file, or a last frame
   that fails its checksum, within one maximum frame of the end, and only when no whole record
   that would come next starts anywhere after it. A damaged length field or a zeroed region
   (a common failure of flash media at power loss) reads like a torn frame; the records
   committed after it show that it is damage, and the journal is then corrupt and left
   untouched, so its write-ahead markers are never cut away. A frame that fails its
   checksum with more data after it, a record that does not decode or is out of sequence, a
   header that does not read back, another job's header or a format version this build does
   not know is an error, never skipped or repaired. The header is synced under a temporary name
   and then linked to the journal's name, so a journal file always has its header. Only the
   journal's writer opens it for writing (and may cut a torn tail); any other reader, such as a
   summary or an audit upload, reads it without changing it.
6. **The journal owns checkpoints and resume counts.** `VmState`'s `checkpoint` and
   `resume_count` (ADR-233 item 3 left their keeping to the journal) are not used; the VM state
   travels as an opaque postcard blob a step record may carry, and reads back with the step it
   was taken after.
7. **The journal only records.** Committing a marker before the request it guards, ending the
   job when a commit fails, and the restart order itself belong to the job runner. The
   journal's directory is a parameter; stage identifiers are opaque numbers the IR gives a
   meaning.

## Consequences

- Every block costs a sync. On slow storage this can bound the transfer rate; the semantics
  allow block records to be synced in batches (block checkpoints are progress, ADR-229 item 3),
  which needs no format change.
- A checksum failure in the very last frame is indistinguishable from a torn write and is cut
  off. A synced frame is not torn by a crash, so this needs damage to the storage itself.
- Later records are a run of whole records with consecutive numbers after the frame that does
  not read back. Beyond the extent that frame's own length claims, any such run counts, so a
  newest commit torn after earlier damage does not hide the records in between. Inside that
  extent (a VM state can hold any bytes, so a torn record's payload can contain whole frames),
  a run counts only if it ends exactly at the end of the file; a torn record cut exactly at the
  end of such a nested run reads as corrupt, the cautious outcome, since a corrupt journal
  stops the job rather than losing committed markers. Damage that leaves a length pointing past
  later records and a newest commit torn at the same time still cuts those records.
- Durability on Windows rests on `FlushFileBuffers` of the new file also covering its directory
  entry (NTFS logs the entry with the file's metadata). The crash tests end the process, which
  checks the format's recovery, not the storage under a power loss.
- Creating a journal needs hard links (ext4, NTFS); a file system without them (FAT, exFAT)
  fails the creation.
- Nothing locks the file (a lock in `std` needs a newer Rust than the workspace's minimum): one
  writer per job and generation is the job scheduler's duty, and a second writer cutting a
  frame the first is still writing would lose it.
- Poisoning lasts as long as the open journal. After a failed sync, the agent ends the job; an
  agent restarted in the same boot may read back a frame the OS still caches but never wrote,
  which at worst shows a write-ahead marker for a request that was not sent, so the restart
  takes the more cautious path.
- A new-generation journal starts empty; starting it from a handover's checkpoint summary
  needs a record that carries the summary, added with the handover itself.
- Journal protection (design 5.5) fits the format: a value in the header's protection field and
  a per-frame MAC and encrypted payload, with the length and checksum left in the clear so a
  torn tail can be cut off without the key.
- A record is at most 1 MiB, which also bounds a VM state blob in one step record.
