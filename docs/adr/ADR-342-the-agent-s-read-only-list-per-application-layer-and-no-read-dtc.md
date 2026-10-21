# ADR-342: The Agent's Read-Only List per Application Layer, and No `ReadDtc`

**Date:** 2026-10-21
**Status:** Accepted
**Affects:** `agent` (`src/policy.rs`, `src/host.rs`, `src/link.rs`), ADR-309 item 4, ADR-321 item 6,
ADR-235 items 2 and 8, ADR-322 Consequences, ADR-328 item 2 and Consequences, ADR-333

## Context

ADR-309 item 4 replaces the release agent's single UDS allowlist (ADR-235 item 8) with one list of
read-only service identifiers per application layer, kept in agent code and applied at both of the
rule's checkpoints. It also replaces `ReadDtc` with the generic send-receive, for which the host
builds no request bytes. The agent still had one list, `READ_ONLY_SERVICES`, and its host still
built the ReadDTCInformation request for `ReadDtc`.

ADR-321 item 6 keeps `ReadDtc` in the IR encoding, since removing a variant would change the encoding
of every later one (ADR-233). It also kept the agent serving the instruction for programs that
already used it. The frontend never emits `ReadDtc`: `diag.read_dtc` lowers through the profile to a
send-receive. No released agent or stored program depends on the instruction.

Three questions were left open by ADR-309 item 4:
- which application layer a check uses at each checkpoint;
- what a read-only job may send on a layer the agent has no list for;
- what the agent does with a `ReadDtc` once its host no longer builds the request.

## Decision

1. **One list per application layer, in agent code.**
   - `policy::ApplicationLayer` names each layer the agent has a list for, starting with UDS.
     `ApplicationLayer::named` reads a protocol declaration's layer name (`Protocol.application_layer`,
     ADR-321 item 4).
   - `ApplicationLayer::read_only_services` gives that layer's list by an exhaustive match. Adding a
     layer therefore fails to compile until its list is written, as ADR-309 item 4 requires. UDS's list
     is `UDS_READ_ONLY_SERVICES`, unchanged: ReadDTCInformation and ReadDataByIdentifier.
   - The program never names its own read-only services.
2. **The layer at each checkpoint.**
   - **Before the link opens**, `policy::check_program` judges every request by the layer every
     protocol of the job's variant names (`ApplicationLayer::of_job`). A program without a document
     runs on a link its caller built. `LinkConfig::validate` admits only UDS on CAN there, so such a
     program is judged as UDS.
   - **The host** checks every request it sends against the link's layer (`Link`'s
     `application_layer`). That layer comes from `LinkConfig::application_layer`, which is UDS for the
     one protocol `validate` accepts. A link opened from a document is UDS on CAN as well, since
     `LinkConfig::declared` refuses any other (ADR-324).
3. **No layer, no list.** The layer is unknown when a variant declares no protocol, names a layer the
   agent has no list for, or names different layers on different protocols (which ADR-309 item 3
   forbids). A read-only job may then send nothing, not even a read: the check fails closed. The
   simulator permission of a debug build (ADR-247, ADR-331) has no list and is unchanged.
4. **`ReadDtc` is refused, and the host builds no request for it.**
   - `policy::check_program` refuses the instruction as unsupported before anything opens, whatever
     the permission.
   - `WorkerHost::read_dtc` answers `Unsupported` instead of sending ReadDTCInformation, and
     `read_dtc_bytes` is gone.
   - This supersedes ADR-321 item 6's "the agent keeps serving it". The dry run (ADR-328 item 2) still builds the instruction's UDS bytes, now the only place that does. The encoding is unchanged:
     `ReadDtc` stays an `Op` variant, the VM still executes it against any `DiagHost`, and the dry
     run and scripted test hosts may still answer it.

## Consequences

- A program from before ADR-309 that uses `ReadDtc` no longer runs on the agent. It fails at the
  first check, before the link opens, and has to be converted again from its source.
- A non-UDS profile (ADR-309 Consequences) needs an `ApplicationLayer` variant with its list, and a
  link that carries that layer. Until both exist, every request of such a variant is refused at the
  first checkpoint, even a read.
- The two checkpoints of ADR-235 item 8 stay. The later read-only exceptions keep to their own ADRs
  and are not part of this change:
  - the read-only ceiling widened inside access programs (ADR-325 item 10);
  - TesterPresent for an identification job (ADR-327).
- `policy::check_service` and `check_request` take the layer next to the permission.
- A program that only reads, on a variant whose layer has no list, counts as writing (`policy::writes`), since a read-only job may send none of its requests. Its job asks for the reprogramming slot, and a debug build without one refuses it as `NoReprogrammingSlot` before the link would be refused. Either way nothing is sent.
