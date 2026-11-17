# ADR-488: Confirmation Defaults Are a Product-Wide Table and Roles Stay a CHECK List

**Date:** 2026-11-17
**Status:** Accepted
**Affects:** design 5.1, 6.3, 6.8; `db/migrations/0012_confirmation_defaults.sql`, `db/README.md`; `crates/server` (`jobs`), `crates/server/docs/ngr-server.md`; details ADR-477 item 7

## Context

`POST /v1/jobs` (ADR-477) took a job's confirmation level from a constant in code, until the
database held the defaults of design 6.3. The same milestone asks for seed data: the roles, the
default level and the preset default. Open: where the defaults live, what they hold, what may
override them later, and whether roles and the preset default become rows too.

## Decision

1. **A table `confirmation_defaults (kind, level)` holds the defaults.** It is product-wide, with
   no tenant column, and both columns carry the same `CHECK` lists as `jobs.kind` and
   `jobs.confirmation_level`. Migration 0012 seeds it: `acquire`, `monitorSession` and
   `monitorCapture` with `notifyOnly`, `runSequence` and `writeSettings` with `gracePeriod`,
   `reprogram` with `onSiteApproval`, as design 5.1's table gives them. `identify` has no row,
   since no route creates it.
2. **A job creation reads its level from the table** in its own transaction, and a kind without a
   row fails the creation as 500 with nothing inserted: a missing row is a fault of the
   installation, not of the request. The constant in code is removed.
3. **The design's firmware write to an unattended device is not a kind.** Its two-person approval
   and execution time window are a condition of the agent and are not seeded here.
4. **Organization and per-agent policies (design 6.3) are separate and may only tighten** these
   defaults: a policy never yields a level looser than the row of the kind.
5. **Roles stay the `CHECK` list on `operator_roles.role`**, listed with their meaning in
   `db/README.md`. The list is part of the permission model that the code checks by name, so a
   row-per-role table would add a second place to keep in step without letting an operator define
   a role the code could act on. The preset default stays the column default of `agents.preset`
   (`personal`), documented.

## Consequences

- A change of a default is an `UPDATE` of one row; the next created job takes it, and jobs
  already created keep their level.
- A new job kind needs a migration that adds its `CHECK` entry and its row before a route may
  create it.
- The level is read after the active-key check and before the signing, so a request that reaches
  that step without a row fails as 500 and signs nothing. A request that loses the key race to a
  concurrent winner can get that 500 instead of the replay, since it reads the table before its
  insert finds the winner's row. A replay that the first read of the key finds is answered before
  the table is read.
- Tightening policies are a later item.
