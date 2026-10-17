# ADR-314: Application-Layer Profiles as Typed Frontend Data

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `diag-frontend` (`src/profile.rs`), `crates/diag-frontend/docs/starlark-subset.md`
3.2 and 3.4, `crates/diag-frontend/docs/csv-tables.md` 5.3; interprets ADR-309 items 2 and 5

## Context

ADR-309 item 2 has the frontend hold one profile per application layer, as data, saying for each
protocol-neutral call (`diag.read_dtc`, `diag.routine`, `diag.security_access`) which services it
expands to, how their responses map to the call's result under names every profile shares, and
which negative-response shape and pending codes the layer uses. It leaves open how a profile is
written and what the shared result names are. ADR-310's notation could describe a profile's
services, but its parser does not exist yet. The maintainer chose typed Rust data for now.

Two further questions follow. A profile must find the variant's declared service a call means,
whose response decodes the result, without depending on the names a definition gives its
constants and fields. And routine results differ per routine, so not every result can carry
shared names.

## Decision

1. **Typed data in the frontend.** A profile is a `static` `profile::Profile` in `diag-frontend`,
   built from `&'static` slices with no parser: the layer's name, its calls, its negative-response
   shape and its pending codes. Each call is a sequence of steps; a step names the instruction it
   lowers to (the generic send-receive, `RoutineControl`, or the `SecurityAccess` seed-to-key
   step), its request template, whether its response is decoded through a declared service, and
   how that response maps to the result. `profile::get(layer)` finds a shipped profile. A later
   text form, such as ADR-310's notation, would be read into the same types.
2. **Templates select by value and position.** A request template is the service identifier
   followed by parts in wire order: a constant the profile fixes, a call argument that must equal
   the declared constant at that position (a routine's sub-function and identifier), a call
   argument encoded into the declared field at that position (a DTC status mask, a security level,
   a key), and the call's `params`, whose keys are the remaining declared request fields as for
   `diag.request` (`csv-tables.md` 5.3). The declared service is found against the ADR-313 index
   by these values, never by the names a definition gives its constants.
3. **Results map by position.** A shared result name points at a top-level field of the declared
   response, counted after its constants, and the lowering checks that field's kind and width at
   ingestion, so a definition keeps its own field names and a mismatch names the field.
4. **Shared result names.** `diag.read_dtc` gives `mask` (the availability status mask) and
   `dtcs` (a list of dicts with `code` and `status`), read from the response's mask field and its
   repeat-until-end field of code-then-status records. `diag.routine` shares no names: its
   `fields` are the declared names of the last step's response, as for `diag.request`, since
   routine results are routine-specific. `diag.security_access` has no `fields`; its `raw` is the
   key response.
5. **The worker limits are checked by `validate`.** `Profile::validate` rejects a negative shape
   other than `7F`, the service identifier and the code, and a pending code outside 0x78, 0x21 and
   0x23 (ADR-309 item 5), and it checks that every call can be lowered: each intent at most once;
   constants and arguments of whole bytes, at most 64 bits, constants fitting their width, and an
   argument that runs to the end of the request last; each call argument used exactly once and only
   by its own call, with only a routine's identifier and sub-function selecting a service; `params`
   once and last for `diag.routine`; `RoutineControl` only for `diag.routine` of the UDS profile,
   with SID 0x31 and the sub-function and the 16-bit routine identifier first (the instruction
   sends UDS's request itself, ADR-309 item 4), and on UDS `diag.routine` as that one step, which
   recovery relies on; for `diag.security_access`, one key step that sends nothing and directly
   follows the step whose undeclared response gives the seed, the level on that seed request, and
   the level plus one and the key together in the one request that directly follows the key step
   and ends the call; and the results of item 4: one DTC map with two different positions for
   `diag.read_dtc`, on the request that carries the mask, declared names on the last, declared step
   of `diag.routine` only. A test also checks that no two shipped profiles name the same layer.
   Profiles ship with the frontend, so a profile that fails is a programming error caught by a test
   over every shipped profile, not an ingestion error; ingestion errors concern only a variant (an
   intent its profile lacks, no single declared service, a declared response that does not fit the
   map).

## Consequences

- Adding or correcting a profile is a code change and a frontend release, as ADR-309 already
  expects; authors cannot ship their own profile.
- `dtcs` is a list, which neither the IR's run-time values nor its decoding hold; ADR-323 reads
  it record by record from the response bytes instead.
- A definition whose DTC response declares an extra field before the mask or the record list does
  not fit the UDS map; the error names the field. The rule for which declared service a
  `diag.read_dtc` response decodes through moves from `csv-tables.md` 5.3 into the profile.
- Selecting by value ignores the names `sub` and `rid` that `routines.csv` generates; the
  constants are matched in order.
