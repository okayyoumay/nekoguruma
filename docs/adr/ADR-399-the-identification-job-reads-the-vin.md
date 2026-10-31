# ADR-399: The Identification Job Reads the VIN, Establishes It on Agreement and Takes the Vehicle's Lock

**Date:** 2026-10-31
**Status:** Accepted
**Affects:** `agent` (`identification`: `check_plan`, `PlanRefused::VinSource`, the VIN phase of
`run_identification`, the report; `inputs`: `vin_source_field`, `field_bytes_of`; `runner`:
`promote_to_vehicle`), `crates/agent/docs/ngr-agent.md`, design 8.8.2, `docs/glossary.md`;
ADR-261 item 3, ADR-262, ADR-327 item 5, ADR-345, ADR-392, ADR-393, ADR-395, ADR-396, ADR-397

## Context

ADR-327 item 5 has the identification job read the VIN after the presence requests, when a
candidate or a definition chosen by hand declares a VIN rule, and ADR-395 and ADR-396 built the
plan, its checks and the presence phase. The job did not read the VIN yet. This ADR records how
the reads are made, grouped and decided, what the job reports, and how the established VIN takes
the per-vehicle lock. Narrowing the candidates by the VIN rules and the reads of the ECU
definitions are separate steps.

## Decision

1. **The read unit.** A read is one endpoint, the ComParams of the declared link other than the
   timings (equal, as for a presence group, ADR-395 item 6) and one encoded request. The VIN
   sources of all candidates' VIN rules (a definition chosen by hand is the plan's one
   candidate) are grouped into reads in the order their first source comes in the plan; a source
   named by several rules is one source. A read opens one link with the first source's declared
   link, the timings raised to the largest of its sources' links (`raise_timings`, the step a
   presence group takes its timings by), sends the request with the origin `identification` and
   decodes the one response with the declaration of every source of the read. The links are
   opened one at a time, after the last presence link is closed.
2. **Sources are checked before any link opens.** `check_plan` builds each VIN source's table
   entry through the ADR-345 code (`inputs::vin_source_field`: the declared field in the table's
   shape, which `ServiceSources::from_document` also uses for the VIN role, and 17 bytes of
   ASCII text) and the table's own check of one entry (a single-identifier read, offset 2). A
   source that fails is `PlanRefused::VinSource`, naming the document, service and field. The
   service is read-only and declared by the checks that already run (ADR-327 item 1), and each
   request leaves through the host's policy check (ADR-235 item 8) as every other does.
3. **Agreement.** A read is accepted when every declaration of it decodes the response to one
   well-formed VIN (ADR-261 item 3; a declaration that is a negative response, does not decode or
   gives text that is not well-formed fails it). The job's VIN is established when at least one
   read is accepted and every accepted read gives the same VIN, and the VIN phase was not
   interrupted (item 5); reads that gave nothing do not block it, and two that differ establish
   none and are both reported (ADR-327 item 5). What each source gave is `Decoded`,
   `NotWellFormed`, `Undecodable`, `Negative` (with the response code), `NoAnswer`,
   `LinkRefused` or `NotRead`. A source whose document's declared link ADR-324 refuses is
   `LinkRefused`: nothing is opened for it and the other sources decide (ADR-395 item 3).
4. **The vehicle's lock.** After the last VIN link is closed, an established VIN takes the
   per-vehicle lock on the job's guards (`promote_to_vehicle`, the step the restart and the
   gates of a procedure promote by; `JobGuards::take_vehicle` allows guards without the slot).
   When another job holds the vehicle the run waits until the holder lets go, or until the
   cancel (design 8.8.1), with no VCI link open. The report says whether the lock is held; the
   caller's guards release it when they are dropped.
5. **Loss and bounds.** The VIN reads follow the presence phase's rules through the same code
   (`open_part`, `repeat_within_bounds`): a link opened again within the run's bounds
   (ADR-393, ADR-394), the request sent again within `max_primitive_repeats` with the origin
   `repeat` (ADR-392), and the reopens counted over the whole run (ADR-396 item 6). A read is
   idempotent, so a receive error sends it again too (the presence request is not sent again on
   one). A request nobody answers within the repeats is `NoAnswer` and blocks nothing; a
   negative response is `Negative` and is not sent again. When the bounds run out in a read, the
   remaining reads are `NotRead` and `vin_interrupted` is set; no VIN is established and no lock
   is taken, though an earlier read's accepted VIN stays in `vins_seen`. A presence phase
   whose bounds ran out leaves the VIN phase unrun the same way. A cancel, also during the wait
   of item 4, ends the run in `JobError::Cancelled`.
6. **The VIN rules are evaluated and nothing is dropped.** For an established VIN the report
   gives each candidate's VIN rule as matched or not matched (the positions of the rule against
   its expected text); with none, every rule is unevaluated. No candidate is dropped on the
   result: narrowing by the rules is a separate step, and the server recomputes the candidates
   from the facts (ADR-327 item 6).
7. **The report.** `IdentificationReport` gains `vin`, `vins_seen` (each distinct accepted VIN in
   the order read), `vin_reads` (the endpoint, the request in hex, the sources with what each
   gave, and the accepted VIN), `vin_interrupted`, `vehicle_lock_held` and `vin_rules`. Only
   `run_identification` writes them. The VINs are `journal::Vin` values, whose `Debug` hides the
   text; the JSON report carries them as text, as it is the job's result. The VIN is written to
   the communication log's `identity` event as each declaration reads it (design 8.11), and
   never to the operational log (design 12.2).

## Consequences

- `declared_field` requires the VIN field to end the response: a VIN in a data identifier with
  fields after it, and a request of any other shape than a single-identifier read, cannot be a VIN
  source and the plan is refused. Where a real definition needs one, the table's shape has to grow,
  for the restart's identity reads as well.
- Because the table admits only a 17-byte text field that ends the response, the declarations of
  one read cannot decode different VINs from the response; the rule of item 3 guards against a
  table that admits more.
- Only the VIN rules' sources are read; a rule's component values, the field rules and the ECU
  definitions' variant reads are still to come, and so is narrowing by the VIN.
- The identification run, which ADR-396 item 7 has take the per-VCI lock only and send only the
  presence requests, now sends read-only VIN requests and may hold the vehicle's lock. A caller
  that waits for a held vehicle sees the run wait there; the cancel ends it.
- The VIN reaches the report, the communication log and the caller's output, which are the places
  design 16.2 already protects for a VIN.
