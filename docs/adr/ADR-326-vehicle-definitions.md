# ADR-326: Vehicle Definitions for Model Identification and Topology

**Date:** 2026-10-18
**Status:** Accepted (items 2, 3 and 5 and the Consequences amended by ADR-327: a VIN matching rule, and a read-field matching rule, names the service and field its value is read from as one variant document declares them, and an identified variant that contradicts a candidate's topology is a vehicle-derived value for the candidates and the scope check; when several alternatives of an entry match, the entry is ambiguous and the operator chooses; the agent runs identification from a signed plan and guards jobs that change an ECU, so its variant identification and runtime, the IR (variant attributes, definitive negative responses) and the signed job inputs change)
**Affects:** design 8.1 (L3), 8.3 (ODX subset), 8.8.2 (new), 9.2; `diag-frontend` (a new
definition kind), `server` (identification, catalog, prefetch), `db/migrations/`
(`operator_roles.scope`), `schemas/record-template.schema.json` (`appliesTo.models`),
`api/openapi.yaml` (SOVD components); `docs/glossary.md`

## Context

Vehicle knowledge (L3, design 8.1) is defined per ECU: an ECU definition holds its variants,
their identification patterns (8.9.1, ADR-306, ADR-315), its protocol and link parameters
(ADR-324), its services and its procedures, and a job carries the documents of the ECU
definitions it runs (ADR-318). Nothing describes a vehicle as a whole.

Yet several parts of the design already speak of vehicle models:

- operator roles can be scoped by vehicle model or OEM (design 6.8, `operator_roles.scope` in
  `db/migrations/0001_init.sql`);
- a record template can apply to vehicle models (`appliesTo.models` in
  `schemas/record-template.schema.json`);
- prefetching for offline work names vehicle models (design 4.1), and the first sync fetches
  model-specific IR once the target is known (design 9.4);
- the SOVD `components` resource is the VCI → vehicle → ECU hierarchy (design 8.7).

None of them says what a vehicle model is, how a job's vehicle is mapped to one, or where the
list of ECUs fitted to a model is kept. Vehicle-wide work (a DTC scan and clear over every ECU,
showing an operator what a vehicle carries, finding the gateway an ECU is reached through)
needs that list: which ECUs a model has, how each is reached (directly, or behind which
gateway), and which ECUs are alternatives of one another (optional equipment, supplier
changes).

ODX models exactly this in its vehicle information specification (ISO 22901-1 (2008) 7.3.10),
in two parts: identification, a set of information components (OEM, vehicle model, model year,
vehicle type) whose values narrow the possible vehicle topologies and come either from the
technician or from values read from the vehicle; and topology, which describes per set of
vehicles the connectors, physical links and logical links (to a gateway or to a member ECU,
possibly through a gateway), with groups of base variants that are alternatives. The design's
ODX subset (8.3) neither includes nor excludes it.

## Decision

1. **A vehicle definition is a kind of L3 definition of its own**, beside ECU definitions,
   modelled on ODX's vehicle information specification. It has an identification part and a
   topology part, and an identifier unique among vehicle definitions. ECU definitions stay as
   they are; a vehicle definition refers to them by their identifiers and never copies them, so
   one ECU definition serves every vehicle definition that fits it.
2. **Identification.** A vehicle definition states the values of four information components:
   OEM, vehicle model, model year and vehicle type; each may list several values (a definition
   that covers several model years). Each component's value for a job's vehicle comes from the
   operator, or from the vehicle by matching rules the definition declares: a position range of
   the VIN compared with expected text, or a field of a read service of one of its ECUs,
   compared as variant patterns compare (ADR-306). A definition is a candidate when no known
   value contradicts it. One candidate is selected; several are offered to the operator, who
   picks one; none leaves the vehicle without a definition, and the operator either picks one
   or works with ECU definitions and generic packages (such as generic OBD, ADR-237) alone.
   The selected definition's identifier is recorded with the job and its data; it carries no
   personal data, unlike the VIN (design 16.2).
3. **Topology.** A vehicle definition lists its ECU entries. Each entry has a role name unique
   in the definition (`engine`, `gateway`), the ECU definitions that may fill it, in order, and
   whether the entry is optional. An entry with several definitions is a group of alternatives
   (ODX's ECU group): the fitted one is the first whose variant identification (8.9.1) selects a
   variant. When none does, the entry's state says why: if an ECU answered an identification
   request but no variant matched, the entry is reported as an unrecognized variant (the same
   case as the `VARIANT_MISMATCH` failure), so an installed but unsupported ECU is never hidden
   as absent; only when no identification request was answered is a mandatory entry reported as
   not answering and an optional one as not fitted. An entry may name the entry of the gateway it is reached through; the gateway
   relation is acyclic and every gateway named is an entry of the same definition.
4. **Link parameters stay with the ECU definition.** How an ECU is reached (protocol, pins,
   identifiers, baud rate, ComParams) is its definition's declared protocol (ADR-324); a vehicle
   definition does not override it. An ECU that is reached differently in two vehicles is two
   ECU definitions. The ODX frontend folds a logical link's protocol stack and ComParams, and
   its physical link's, into the ECU definition it produces for that link, so links to one base
   variant with different parameters produce separate ECU definitions.
5. **The server and the UI use vehicle definitions; the agent does not.** The server keeps them
   with the other L3 definitions, identifies a job's vehicle by item 2, builds the SOVD
   `components` hierarchy and the vehicle view from the topology, expands prefetching for a
   vehicle model to the manifests of the ECU definitions its definitions refer to, and resolves
   the model and OEM scopes of roles and record templates against the identification values:
   `models` names vehicle-model values and `oem` OEM values, so one scope covers every
   definition of that model. Only values established from the vehicle (a VIN matching rule on
   the VIN the job is bound to, or a field read from its ECUs) count for a role's scope: a
   scoped role authorizes a job only when every candidate those values leave is inside its
   scope, so an operator's entry or choice among candidates can narrow the selection but never
   widen what a role permits (design 6.8); a vehicle no such value identifies needs a role with
   no model or OEM scope. Record templates, which grant nothing, may use the selected
   definition whichever way it was chosen. A job still carries the ECU documents it runs (ADR-318);
   the agent's runtime, its variant identification and its locking (8.8) do not change.
6. **Supply formats.** The proprietary CSV format gets a vehicle definition directory, one
   per vehicle definition, with fixed tables for the identification values, the matching
   rules and the ECU entries, converted by `diag-frontend` and checked in stages like ECU
   definitions (ADR-298); its tables are specified with their implementation. The ODX
   frontend converts the vehicle information specification's information components and
   their matching components that refer to diagnostic services (a reference to a multiple-ECU
   job, ISO 22901-1 (2008) 7.3.11, is refused at ingestion, as such jobs are, design 8.3), and the logical links and ECU
   groups of each vehicle information as entries; connectors and pins are not kept beyond what
   item 4 folds into ECU definitions.

## Consequences

- The vehicle-model references in design 4.1, 6.8, 8.7 and 9.4, in `operator_roles.scope` and in
  `appliesTo.models` now have a meaning, which their implementations must follow.
- A vehicle definition adds no runtime path to the agent, so the IR schema and the trust model
  for documents do not change; vehicle definitions are signed and distributed to the server's
  consumers like the other definitions, but agents do not fetch them.
- Identification from values read from the vehicle needs a read-only job that reports the VIN
  and the declared fields; it uses the agent's existing reads, and the server decides.
- Item 4 duplicates an ECU definition for every distinct way it is reached. If that becomes a
  burden, per-entry link parameters can be added in a later ADR without changing items 1 to 3.
- Procedures keep naming the ECU definitions they address; a procedure written per vehicle (a
  vehicle-wide DTC scan over every fitted entry, or ECUs resolved by role name through the
  topology) is a later decision.
- Follow-up work (the CSV format and conversion, server identification and use, the ODX
  conversion) is in the backlog.
