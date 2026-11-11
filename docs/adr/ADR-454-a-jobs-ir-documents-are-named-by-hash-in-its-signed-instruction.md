# ADR-454: A Job's IR Documents Are Named by Content Hash in Its Signed Instruction

**Date:** 2026-11-11
**Status:** Accepted
**Affects:** design 8.2.6, 11.1; `agent` (`runner.rs` `check_job`, `document.rs`), the server's job delivery (M3)

## Context

A job runs one variant's IR document, or the files of every combination it may meet (design 8.2.6, ADR-318, ADR-325). Today `ngr-agent run` reads such a document from a local file and `check_job` verifies it as a document: its structure and schema version (ADR-296), its declarations against the program, and, for a multi-target procedure, the hashes the files name of one another (ADR-325). Nothing in that check says who supplied the document. Extension packages that carry IR are signed with the operator's distribution key (design 9.4, 11.1), but package signing is M6 work, so the M3 server has no signed source for a job's documents.

## Decision

1. The signed job instruction (design 11.1: the server's instruction key, received at registration, which M3 brings) names each document of the job by its content hash.
2. The agent runs only a document whose content hash the instruction names. A document the instruction does not name, or whose bytes do not hash to a named value, is refused before anything is sent, with the other checks of `check_job`. The hash check adds to those checks and replaces none of them.
3. No new key is introduced: the instruction key that guards the job's target, vehicle, approval record and expiry guards the documents through their hashes.
4. M6's package signature (the distribution key, design 9.4, 11.1) adds to the same check rather than replacing it: a document will then also have to come from a package signed with the distribution key. The instruction's naming of hashes stays, since it is what ties a document to this job and approval.
5. Rejected: ingesting unsigned documents on the local profile. Other OS users on the PC can reach the server there (design 13.3), so an unsigned document would let one of them put a program in front of a vehicle.
6. Rejected: a development signing key. It would need its own generation, storage and rotation, and M6 would discard all of it.

## Consequences

- The server must hand the agent the documents together with an instruction that names them; the delivery and its test are an M3 item.
- A compromised server that holds the instruction key can still name any document; the distribution key's separate root (design 11.1) is what closes that in M6.
- Content addressing (design 8.2.6) already identifies each file by its hash, so the hashes the instruction names are the same ones the manifest of a package holds.
