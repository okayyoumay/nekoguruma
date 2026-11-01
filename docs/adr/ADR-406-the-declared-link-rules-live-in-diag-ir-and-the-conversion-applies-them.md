# ADR-406: The Declared-Link Rules Live in `diag-ir` and the Conversion Applies Them

**Date:** 2026-11-01
**Status:** Accepted
**Affects:** `diag-ir` (`src/link.rs`), `agent` (`src/link.rs`: `LinkConfig::declared`,
`LinkConfig::validate`), `diag-frontend` (`src/tables/resolve.rs`, `src/tables/rows.rs`,
`src/convert.rs`, `docs/csv-tables.md` 4.3), `crates/agent/docs/ngr-agent.md`, design 8.2.4;
supersedes ADR-324's consequence that the frontend accepts what the agent refuses, amends ADR-295
(`comparams.csv`), applies ADR-397

## Context

ADR-324 put the rules for a document's link in the agent. Its consequences admit that the
frontend accepts definitions the agent refuses (a ComParam with a unit or one the link sets
itself, a negative value, several protocols per variant), so an author learns of them only when a
job starts. ADR-397 closes a parity gap by having both paths call one function rather than copy
it, and `diag-ir` already holds rules both sides share (`is_standard_seed_level`,
`read_only_services`, `DEFAULT_MAX_STEPS`). Every rule `declared` and `validate` apply to a
physical link is static: name lists, ranges and counts.

## Decision

1. **`diag_ir::link` holds the declared-link rule set**: the protocol names, the lists of
   ComParams the link sets itself or that let a primitive outlast the host (`SET_BY_LINK`,
   `OUTLAST_THE_HOST`), the identifier and timing bounds, `only_protocol`, `check_protocol` (every
   refusal of one protocol, in the IR's own units) and `further_comparam`. It ships no table of
   the standard's ComParam names.
2. **The agent builds its link from `check_protocol`'s result.** `LinkConfig::declared` rounds
   the timings up to milliseconds and fills the defaults; `validate` judges further ComParams
   through `further_comparam`. `declared` also refuses a functional identifier wider than 11 bits,
   for every job kind; the identification plan keeps ADR-395's whole-plan refusal of such an
   identifier (`BadFunctionalId`), made before any link is judged.
3. **The conversion applies the same functions** in the resolve stage, once per variant and
   protocol row, and reports each refusal at the row and column that caused it: `CSV_UNSUPPORTED`
   for a construct the link does not support (a protocol that is not UDS on CAN, extended
   addressing, a second protocol, a ComParam the link sets or that outlasts the host),
   `CSV_BAD_VALUE` for a value or name outside the rule (a timing of 0 or above 4294967000 us, a
   name without the `CP_` prefix; a timing value under 1 ms but above 0 is rounded up to 1 ms), `CSV_DUPLICATE` for a ComParam declared twice ignoring ASCII
   case. No new error code. A row whose `variants` list names an unknown variant 
   takes part in the variants it does name.
4. **A variant declares exactly one protocol.** The rule that a variant's protocols share an
   application layer, with its `CSV_DUPLICATE`, is retired: it cannot fire.
5. **`comparams.csv` has no `unit` column**, and `value` is bounded at reading to 0 to
   2^32 - 1. The IR keeps `ComParam.unit_id` and `int64 value` for other frontends; the agent
   still refuses a unit and a value outside 32 bits unsigned in a document.
6. **What needs the worker or several documents stays at run time**: a ComParam name the worker
   does not know, two names it resolves to one id, a value it refuses, the functional-link rules
   (ADR-396) and ADR-395's cross-document checks.
7. **A test asserts that `SET_BY_LINK` equals the names the link writes** (its own ComParams, the
   indications and the two identifiers of the receive table), so the list cannot drift from its
   writers.

## Consequences

A definition the agent cannot open fails at conversion, naming the row and column. The lists have
one writer, and a new protocol or ComParam rule is one change in `diag-ir` plus the format and
agent documents. Agent messages for a refused list name now name the ComParam. A document with
two protocols for one variant cannot be written until a job can choose a link (ADR-324
consequence 4 stands). An existing definition with a `unit` column or a second protocol per
variant must be edited; the example ECU needs no change, so the IR golden files are unchanged.
The conversion cannot check a ComParam name against the standard's table, so a misspelled
`CP_` name still fails when the link is set up.
Agent messages for a refused link are worded differently and may name another refusal first than
before, since the rules run in one order in `check_protocol`; every refusal is still a
`HostError::DeclaredLink`. A journaled job whose IR has a functional identifier above 0x7FF now
fails `declared` on resume (design 8.2.5); no current frontend produces one.
