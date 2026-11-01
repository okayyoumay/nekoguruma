# ADR-401: The Identification Job Reads the ECU Definitions and Reports the Entry States

**Date:** 2026-11-01
**Status:** Accepted
**Affects:** `agent` (`identification`: `check_plan`, `PlanRefused::IdentificationRequest`,
`ecu_phase`, `evaluate_entries`, the report; `inputs`: `negative_code`), `diag-ir` (`identify`:
`identification_requests`, `decode_identification_answer`), `crates/agent/docs/ngr-agent.md`,
design 8.8.2, `docs/glossary.md`; ADR-315 item 3, ADR-324, ADR-326 item 3, ADR-327 items 3, 4, 6,
7 and 8, ADR-395 item 3, ADR-396 item 6, ADR-397, ADR-399 items 4 and 5

## Context

ADR-327 items 3 to 5 have the job read each ECU definition once, each variant over its declared
link, and report every match, the outcome of each request and the entry states; ADR-395, ADR-396
and ADR-399 built the plan, the presence phase and the VIN read. Nothing read a definition yet.
The agent had no variant-identification path (`diag_ir::identify` serves the server and the
hardware checks), and the IR declares no definitive negative response. Narrowing the candidates,
the choice of the next ECU and the rule fields are later steps.

## Decision

1. **The read unit and the link unit.** A read is one definition at one endpoint; a link group is
   the variants of that read whose declared links have equal ComParams other than the timings
   (as for a presence group and a VIN read, ADR-395 item 6, ADR-399 item 1). The plan check
   derives the reads before any link opens, one per (definition, endpoint) in plan order, each
   definition's documents in their listed order. A group opens one link, the first document's
   declared link with the timings raised to the group's largest (`raise_timings`), one at a time
   after the VIN phase and the vehicle's lock. A definition listed twice for a document reads it
   once.
2. **One send per distinct request.** The plan check builds every document's identification
   requests (`diag_ir::identify::identification_requests`, which `identification_document` also
   uses) and groups declarations, a (document, service) pair each, by the request's bytes within
   a group. Each distinct request is sent once and its one response is decoded with every
   declaration that shares it (`decode_identification_answer`, the step `identified` also uses),
   as the VIN read does (ADR-399 item 1). A pattern service whose request cannot be built
   without values is `PlanRefused::IdentificationRequest`, known before any link opens.
3. **Loss and bounds.** The requests follow ADR-396 item 6 and ADR-399 item 5 through the same
   code (`open_part`, `repeat_within_bounds`): the link is opened again and the request sent
   again within the run's bounds, a receive error sends it again (a read is idempotent), a
   negative response (`negative_code`, shared with `field_bytes_of`) is not sent again, and the
   reopens count over the whole run. A request that got no usable answer within the repeats is
   `NoUsableAnswer`. A link the worker refuses at read time marks the group's declarations
   `WorkerRefused` and the next group is read. When the bounds run out, the rest are `NotRead`
   and `ecu_reads_interrupted` is set; an interrupted presence or VIN phase leaves the phase
   unrun, with the flag set when a read would have opened a link.
4. **Conclusive per (definition, endpoint).** A read is conclusive when it has a declaration,
   every declaration decoded a positive response, and no variant of the definition has a link
   ADR-324 refuses. A positive response that does not decode is `Undecodable` and inconclusive
   (ADR-327 item 3). No negative response is definitive yet, since the IR declares none, so any
   `Negative` leaves the read inconclusive; the rule is computed in one place and is the one to
   extend when the IR declares definitive codes. A variant whose link is refused has no endpoint
   (ADR-395 item 3), so it is reported as a read with `endpoint` null whose declarations are
   `LinkRefused`, and it leaves its definition inconclusive at every endpoint the definition is
   read at: the conservative reading, which only withholds an exclusion, since a refusal may
   concern the very identifiers an endpoint would be inferred from.
5. **Every matching variant is reported.** Each variant is judged with `variant_matches` over the
   fields its declarations decoded; all matches go to the read's `matching`, not the first by
   name that `select_variant` picks (ADR-315 item 3 as ADR-327 item 7 amends it). Variants at
   different endpoints of one definition may both match (ADR-327 item 4). A match is a match
   whatever the conclusiveness, which only rules variants out.
6. **Entry states.** Each candidate entry is judged from the reads of its alternatives, and only
   from a phase that ran to its end: when `ecu_reads_interrupted` is set every entry stays
   `NotRead`, as the VIN phase establishes nothing when interrupted. An alternative has had no
   physical read when every one of its declarations is `NotRead` or `WorkerRefused`; an
   alternative with no declaration (no variant has a pattern) is left out, as item 8 has it, and an
   entry with no alternative left is `NotRead`; an entry with such an alternative is `NotRead` whatever the others show, and the matches
   of the others stay listed in `ecu_reads`. A `LinkRefused` alternative counts as an inconclusive
   read (item 4), so it never lets a conclusive read without a match make the entry an
   unrecognized variant; an entry no alternative of which had a physical read is `NotRead`.
   Otherwise exactly
   one alternative with a matching variant is `Identified` (with every matching document of that
   definition); two or more are `Ambiguous` (ADR-327 item 3). With no match, an endpoint is
   answered when a read there got a response; when every read at every answered endpoint is
   conclusive the entry is `UnrecognizedVariant` (those endpoints), and when some answered
   endpoint has an inconclusive read it is `AnsweredNotIdentified`, whether or not another
   endpoint settled, so an unrecognized variant is never reported while an alternative may be
   fitted at another address. With no answer, `NotAnswering` for a mandatory entry and
   `NotFitted` for an optional one (ADR-326 item 3), when a request was sent. `NotRead` is a
   new state beside them for an entry for which no request was sent (the phase was interrupted,
   the link refused by ADR-324 or the worker, or no pattern): ADR-326 item 3's "not answering"
   says the ECU was asked, and an unrun phase must not read as an absent ECU.
7. **Phase order and the marked guards.** Presence, VIN, the vehicle's lock, then the ECU reads,
   which therefore run with the lock held when a VIN was established (design 8.8.1). Guards still
   marked after the VIN phase's last close end the run in `JobError::LinkUnconfirmed` once an ECU
   read has a link to open (ADR-396 item 6: no further link opens, ADR-258); this amends
   ADR-399 item 4, whose report came back with no lock, to the case where no ECU read remains.
8. **A variant without patterns** has no request: it sends nothing, can never match and
   contributes nothing to conclusiveness or to the entry gate of item 6, whether its link is
   refused or not; a definition of
   such variants only opens no link, and its read is reported with no requests, not answered, not
   conclusive and with no match.
9. **Plan validation.** `diag_ir::plan` refuses an entry that lists the same alternative twice,
   as its other duplicate checks refuse a repeated id. This adds to the list of ADR-395 item 3.
10. **No `note_identity`.** The ECU reads do not note identities for the communication log:
    `IdentityKind` names the restart's identity roles (the VIN, the hardware identity), not the
    fields of an identification pattern. The `identity` event stays the VIN's; the decoded ECU
    fields are in the report, and each request's exchange entry carries its bytes.
11. **The report** gains `ecu_reads` (per read: the definition, the endpoint or null, the
   documents, each distinct request with the outcome of each declaration, `matching`,
   `conclusive`, `answered`), `ecu_reads_interrupted` and `entries` (candidate, role, optional,
   alternatives, state). Outcomes are `Decoded` (the fields), `Undecodable`, `Negative` (with
   the code), `NoUsableAnswer`, `LinkRefused`, `WorkerRefused` and `NotRead`. Decoded values are
   the agent's own `FieldValue`, built from the decoder's physical value, so bytes are lowercase
   hex as elsewhere in the report and the IR crate commits to no JSON shape; the report derives
   `PartialEq` without `Eq` (a float value). A decoded field can hold the VIN, so the `Debug` of
   `FieldValue` shows its kind and length only, as the VIN types hide their text (design 16.2);
   the JSON carries the values. `CheckedPlan::report` writes the initial values,
   `ecu_phase` the reads, and `evaluate_entries` the states; nothing else. The server recomputes
   from the facts and nothing is dropped (ADR-327 item 6). A plan for a chosen definition or for
   every ECU reads every definition it names, as a plan of candidates does (ADR-327 item 8).

## Consequences

- Every negative response leaves a read inconclusive until the IR declares definitive codes, and
  a definition without patterns is never conclusive; a pattern service whose request needs values
  makes the plan unusable.
- The job's bus time grows by one link per group and one request per distinct request. The
  definitions are read in the plan's order and none is skipped: presence answers do not order or
  skip reads, and no candidate is dropped by a read, which are the narrowing step's work.
- `ngr-agent identify` can now end in `LinkUnconfirmed` where it returned a report with the mark
  kept, when an ECU read remained.
- The end-to-end presence test over the example ECU also reads the software version.
- A service read by both the VIN phase and the ECU phase (the VIN's data identifier is often also
  a pattern's) is sent twice to one ECU. This is accepted: the phases are separate and share no
  state.
- The plan check does not compare a document's `definition_id` (ADR-363) with the identifier the
  plan lists it under.
