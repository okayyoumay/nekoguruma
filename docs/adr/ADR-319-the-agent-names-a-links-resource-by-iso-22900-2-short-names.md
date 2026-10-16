# ADR-319: The Agent Names a Link's Resource by ISO 22900-2 Short Names

**Date:** 2026-10-16
**Status:** Accepted
**Affects:** `agent` (`src/link.rs`: `PROTOCOL_UDS_ON_CAN`, `BUS_TYPE_DWCAN`, `DWCAN_PINS`,
`resource_data`), `iso22900-mock` (`PDUGetObjectId`), `iso22900-service/tests/agent_launch.rs`,
`crates/agent/docs/ngr-agent.md`, design 8.2.4; ADR-235 item 4, ADR-069

## Context

The agent creates a job's logical link on the resource that `GetResourceIds` returns. It named
that resource in the J2534 worker's terms only: the legacy protocol alias `ISO15765`, with no bus
type and no DLC pins. `iso22900-service` refuses a request without a bus type, and a D-PDU API
implementation resolves only the short names ISO 22900-2 (2022) defines: the resource data of
`PDUGetResourceIds` is a protocol, a bus type and the DLC pins (8.4.25, 10.1.4.8), each given by
the object id of its short name. The J2534 worker resolves those same short names through its
resource table (ADR-069), and the alias as well.

There were two ways: one request that both worker kinds accept, or a name for each API, chosen
by the kind of worker the agent launched.

## Decision

1. **One request, in the standard's names.** The agent names UDS on CAN by its ISO 22900-2
   (2022) short names for every worker kind:
   - protocol `ISO_14229_3_on_ISO_15765_2` (Annex B.1; the standard lists
     `ISO_15765_3_on_ISO_15765_2` as the same stack);
   - bus type `ISO_11898_2_DWCAN`;
   - DLC pin 6 as `HI` and pin 14 as `LOW` (Annex B.2).

   `agent::link::resource_data` builds the request, and `LinkConfig::validate` accepts only that
   protocol.
2. **The J2534 worker resolves them to its UDS on CAN row.** With the bus type and the pins, its
   resource table gives exactly one resource. Its UDS-specific behaviour (the ComParams the agent
   sets, session timing) already applies to that protocol as it did to the alias.
3. **The D-PDU API mock resolves the standard names.** `iso22900-mock` gives object ids to the
   short names above beside its own test names, so the agent's request runs against it.

## Consequences

- No code in the agent depends on the kind of worker for naming a resource. A protocol other
  than UDS on CAN, or a bus other than dual-wire CAN, needs its own names, which the agent does
  not support yet.
- A D-PDU API implementation that lists UDS on CAN only under `ISO_15765_3_on_ISO_15765_2`
  resolves nothing for the agent's request. The standard gives both names for the same stack,
  so the agent may have to try the second name when the first is unknown.
- The rest of the link's set-up (ComParams, the unique response id table, event subscription)
  has still not run against a D-PDU API worker.
