# ADR-324: A Document Job's Link Comes from Its Declared Protocol

**Date:** 2026-10-17
**Status:** Accepted
**Affects:** `agent` (`src/link.rs`: `LinkConfig::declared`, `LinkConfig::params`,
`set_up_link`; `src/runner.rs`: `Job::declared_link`; `src/main.rs`: `--tx-id` / `--rx-id`),
`j2534-0404-service` (`tests/agent_end_to_end.rs`), `crates/agent/docs/ngr-agent.md`,
`crates/diag-frontend/docs/csv-tables.md` 4.3, design 8.2.4; ADR-235 item 4, ADR-309, ADR-319

## Context

ADR-235 item 4 left the job's link to its caller: `LinkConfig::iso15765(tx, rx)` built UDS on
CAN at 500 kbit/s with fixed timings. `ngr-agent run` took the identifiers from `--tx-id` and
`--rx-id`, and the end-to-end tests built the same link for every job, whatever the job's
document declared.

A variant's document already declares its link: `EcuVariant.protocols`, each with a short name,
a baud rate, the physical request and response identifiers, an addressing flag, ComParams by
name and value, and the application layer it carries (ADR-309 item 3). The CSV tables name the
protocol by its transport (`ISO15765`, `csv-tables.md` 4.3) and put the application layer in its
own column. The agent names a link's resource by the ISO 22900-2 (2022) short name of the whole
stack, `ISO_14229_3_on_ISO_15765_2` (ADR-319), and supports only that one.

Open points the declaration does not settle by itself:

- a variant may declare several protocols, but a job opens one link;
- the protocol's name and its layer must be read together to tell UDS on CAN apart;
- the IR has no timing fields: the timings are ComParams, if they are declared at all;
- other declared ComParams (the example ECU's `CP_BlockSize` and `CP_StMin`) were not set on the
  link at all.

## Decision

1. **A document's job opens the link its variant declares** (`LinkConfig::declared`,
   `Job::declared_link`). The variant must declare exactly one protocol. With none, the job
   has no link. With several, choosing one would need a rule no definition gives yet. Both are
   refused before a worker starts. `ngr-agent run --document` takes its link this way, and
   refuses `--tx-id` and `--rx-id`. A program without a document declares no link, so
   `--program` keeps the hand-built link until bare programs are retired.
2. **UDS on CAN, by either naming.** The protocol is accepted as UDS on CAN when its application
   layer is `UDS` and its short name is the transport's `ISO15765` or either ISO 22900-2 (2022)
   name of the stack (Annex B.1: `ISO_14229_3_on_ISO_15765_2`, `ISO_15765_3_on_ISO_15765_2`).
   The link still names its resource `ISO_14229_3_on_ISO_15765_2` (ADR-319). The rest is taken
   as declared:
   - the baud rate and the two identifiers;
   - the functional identifier is not used, since the link sends only physical requests.

   Extended addressing is refused, since the link does not set it. So are a missing layer and
   any other layer, since the agent's read-only policy judges UDS service identifiers (ADR-235
   item 8).
3. **ComParams.**
   - `CP_P2Max`, `CP_P2Star` and `CP_RC78CompletionTimeout` give the link's timings. ISO 22900-2
     time ComParams are in microseconds; the link keeps whole milliseconds, so a value is
     rounded up, and a timeout is never shortened.
   - A timing the protocol does not declare keeps the defaults of `LinkConfig::iso15765`.
   - Every other declared ComParam is set on the link as declared, after the link's own, in
     declaration order (`LinkConfig::params`). The worker judges whether it knows the ComParam
     and its value.
   - Refused:
     - a ComParam the link sets itself (the baud rate, the response-pending handling and its
       byte offset, the two identifiers), since it would override what the link derives from
       the protocol's own fields;
     - a ComParam declared twice;
     - a value outside an unsigned 32-bit integer, the only ComParam type the link sends.
   - `LinkConfig::validate` then applies its existing checks (ADR-235 item 4), and also
     refuses a zero baud rate.

## Consequences

- A definition's link reaches the worker as written, ComParams included. The example ECU's
  block size and separation time were silently dropped before.
- The CSV tables have no ComParam unit conversion: a timing ComParam is written in
  microseconds, as ISO 22900-2 defines it.
- A variant reachable over two protocols cannot run a job until the job can name which link to
  open.
- Bare programs still run on a hand-built link. Retiring `--program`, `--tx-id` and `--rx-id`
  is follow-up work, after which every job's link is declared.
