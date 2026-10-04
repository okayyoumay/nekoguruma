# ADR-042: `PDU_PC_UNIQUE_ID` Class Boundary Enforced Symmetrically Between the Two ComParam RPC Pairs

**Date:** 2026-07-02
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/comparam_support.rs` (`is_param_allowed`,
             `is_unique_id_param`, `is_can_param`, `is_kwp_param`, `is_j1850pwm_param`,
             `is_j1850vpw_param`), `j2534-0404-service/src/service/rpc_link.rs`
             (`rpc_set_com_param`), `j2534-0404-service/src/service/rpc_misc.rs`
             (`rpc_set_unique_resp_id_table`)

## Context

ISO 22900-2 §9.3.3.6 splits `CP_*` ComParams into two disjoint groups by RPC
pair: `PDU_PC_UNIQUE_ID` class params (per-ECU addressing) go exclusively
through `GetUniqueRespIdTable` / `SetUniqueRespIdTable`; every other class
goes exclusively through `GetComParam` / `SetComParam`. Neither RPC pair is
supposed to accept a param from the other's class.

Two independent gaps in `j2534-0404-service` let both directions leak:

1. **`SetComParam` / `GetComParam` accepted `PDU_PC_UNIQUE_ID` params.**
   `comparam_support.rs` tracked `PDU_PC_UNIQUE_ID` class membership per
   protocol family (`CAN_UNIQUE_ID_UNUM32`, `CAN_UNIQUE_ID_BYTES`,
   `KWP_UNIQUE_ID_UNUM32`, `J1850_UNIQUE_ID_UNUM32`, exposed via
   `unique_id_params()`), used only by `GetUniqueRespIdTable`'s template
   response. `is_param_allowed` — the function backing `check_param_allowed`,
   which gates `GetComParam` / `SetComParam` — separately listed the *same*
   param IDs in its own per-protocol allow-lists (`is_can_param`,
   `is_kwp_param`, etc.), so a caller could set e.g. `CP_CanPhysReqId` or
   `CP_MidRespId` through either RPC pair, each writing to separate,
   unsynchronized storage (`LogicalLinkState::working.unum32` vs.
   `LogicalLinkState::unique_resp_id_table`). This was more than a spec
   nicety: nothing in the service consumed the `SetComParam` copy
   (`apply_j2534_params` only forwards params with a `to_j2534_config_id()`
   mapping, and `PDU_PC_UNIQUE_ID` params have none), so a caller using
   `SetComParam` for per-ECU addressing would silently have no effect while
   believing it had configured the tester — a correctness trap.
   `rpc_set_com_param`'s Bytefield match arm additionally listed
   `CP_J1939SourceName` (`PDU_PC_UNIQUE_ID` class) as an accepted bytefield
   param — already unreachable once `check_param_allowed` ran first, but a
   second, misleading copy of the same gap.

2. **`SetUniqueRespIdTable` accepted arbitrary, non-`PDU_PC_UNIQUE_ID`
   params.** `rpc_set_unique_resp_id_table` inserted every `com_param_id` a
   caller supplied into the per-ECU `ComParamSet` with no class check at all
   — a caller could put `CP_Baudrate`, or any other regular ComParam, into a
   `UniqueRespIdTable` entry. This is the mirror image of gap 1: the table is
   specified as `PDU_PC_UNIQUE_ID`-only, but nothing enforced that.

## Decision

Both directions are now validated against the same `unique_id_params()` source
of truth:

- `is_param_allowed` checks `is_unique_id_param(protocol, param_id)`
  immediately after the universal-param check and before delegating to the
  protocol-family allow-list functions. If `param_id` has `PDU_PC_UNIQUE_ID`
  class for `protocol`, `check_param_allowed` returns
  `Status::invalid_argument` unconditionally, regardless of what the
  family-specific allow-list would otherwise say. The now-unreachable
  `PDU_PC_UNIQUE_ID` entries were removed from `is_can_param`, `is_kwp_param`,
  `is_j1850pwm_param`, and `is_j1850vpw_param` (rather than left as dead list
  entries), and `CP_J1939SourceName` was removed from `rpc_set_com_param`'s
  Bytefield match arm for the same reason.
- `rpc_set_unique_resp_id_table` now looks up `unique_id_params(protocol)`
  once per call and validates every `item.com_param_id` in every entry
  against it (the `unum32` list for `Unum32` values, the `bytes` list for
  `Bytefield` values) before accepting the table, returning
  `Status::invalid_argument` on the first param that is not
  `PDU_PC_UNIQUE_ID` class for the CLL's protocol.

`unique_id_params()` itself is unchanged; both fixes reuse it as the single
source of truth for which params belong to which RPC pair, rather than
maintaining separate, driftable lists in each direction.

## Alternatives Considered

1. **Allow `SetComParam` to write `PDU_PC_UNIQUE_ID` params but document that
   they have no effect** — Rejected: a `Status::ok()` response that silently
   does nothing is worse than an explicit `INVALID_ARGUMENT`, and contradicts
   the spec outright rather than just being an incomplete implementation of it.

2. **Synchronize `SetComParam`'s Working-set writes into
   `LogicalLinkState.unique_resp_id_table`** — Would require inventing an
   implicit per-ECU identity for a `SetComParam` call (which has no
   `unique_resp_identifier`), and contradicts the spec's exclusivity
   requirement rather than resolving the ambiguity it exists to avoid.
   Rejected.

3. **Leave `SetUniqueRespIdTable` unvalidated, since a stray non-unique-ID
   param sitting in the table is inert** (only specific `unum32.get(&PARAM_*)`
   lookups by known key are ever read back out of an entry, e.g. by
   `install_point_to_point_fc_filters`) — Rejected: "inert but silently
   accepted" is the same class of correctness trap as gap 1 — a caller could
   believe `SetComParam` was the wrong RPC and try `SetUniqueRespIdTable`
   instead for a regular ComParam, get `Status::ok()`, and have configured
   nothing. Explicit rejection surfaces the mistake immediately.

## Consequences

- `SetComParam(CP_CanPhysReqId, ...)` (and the other eight CAN addressing
  params, the five ECU-addressing params, `CP_MidRespId`,
  `CP_J1939SourceAddress`, and `CP_J1939SourceName`) now return
  `Status::invalid_argument` for every protocol. `GetComParam` is
  symmetrically rejected via the same `check_param_allowed` gate.
- `SetUniqueRespIdTable` now returns `Status::invalid_argument` if any entry
  contains a param that is not `PDU_PC_UNIQUE_ID` class for the CLL's
  protocol (e.g. `CP_Baudrate`), instead of silently accepting and storing it.
- Together, `SetUniqueRespIdTable` / `GetUniqueRespIdTable` are now the only
  way to read or write `PDU_PC_UNIQUE_ID` class params, and `SetComParam` /
  `GetComParam` are the only way to read or write every other class — the two
  RPC pairs' domains are disjoint and mutually enforced, matching ISO
  22900-2 §9.3.3.6. Routing/filter-building behavior in
  `GetUniqueRespIdTable` / `SetUniqueRespIdTable` for well-formed tables is
  otherwise unchanged (ADR-007, ADR-014, ADR-039, ADR-040, ADR-041).
- Any existing caller relying on either previously-unvalidated path (setting
  `PDU_PC_UNIQUE_ID` params via `SetComParam`, which had no effect per the
  Context above, or putting regular ComParams into a `UniqueRespIdTable`
  entry, which was inert) will now see an explicit error instead of a silent
  no-op — a behavior change, but one that surfaces a pre-existing bug on the
  caller's side rather than introducing a new one.
