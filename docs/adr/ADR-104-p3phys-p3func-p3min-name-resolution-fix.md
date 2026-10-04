# ADR-104: Fix CP_P3Phys/CP_P3Func Name Resolution — Not Aliases of CP_P3Min

**Date:** 2026-07-19
**Status:** Accepted
**Affects:** `j2534-0404-service/src/service/names.rs`, `j2534-0404-service/src/service/rpc_link.rs`

## Context

While extending `ParamItem` name resolution to `SetComParam`/
`SetUniqueRespIdTable` (ADR-103), a design review surfaced a pre-existing
bug in `j2534-0404-service`'s ComParam shortname table.

`names.rs::map_comparam_name_native` mapped `"cp_p3min"`, `"cp_p3phys"`,
and `"cp_p3func"` all to the same native `P3_MIN` id, unconditionally. A
second function, `map_comparam_name_for_protocol` (used only by
`GetComParam`), special-cased `"cp_p3phys"`/`"cp_p3func"` to the CAN-context
service-level `PARAM_P3_PHYS`/`PARAM_P3_FUNC` constants when the connected
CLL's protocol was CAN-family, on the stated premise that "KWP has no
separate physical/functional P3 hardware register" and so the native
`P3_MIN` alias was an acceptable stand-in on KWP.

That premise is wrong: `CP_P3Phys`/`CP_P3Func` are CAN-only D-PDU
ComParams with no KWP equivalent, and KWP's own P3 timer already has its
own distinct name, `CP_P3Min`. Treating `CP_P3Phys`/`CP_P3Func` as aliases
of `CP_P3Min` on non-CAN protocols meant a client asking for
`CP_P3Phys` on a KWP CLL got silently redirected to a different parameter
(KWP's `P3_MIN`) instead of being told the param it asked for isn't valid
there. This also violated the CLL-independent name-to-id mapping principle
ADR-103 establishes: the same name resolved to different ids depending on
which CLL it was resolved against.

Separately, `comparam_support::check_param_allowed`'s `is_can_param`/
`is_kwp_param` allowlists (ADR-028's per-protocol support table) already
correctly listed `PARAM_P3_PHYS`/`PARAM_P3_FUNC` as CAN-family-only and
`P3_MIN`/`P3_MAX` as KWP-family-only — the *support* check was already
right; only the *name resolution* layer conflated the two.

## Decision

- `map_comparam_name_native` no longer aliases `"cp_p3phys"`/`"cp_p3func"`
  to `P3_MIN`; only `"cp_p3min"`/`"p3_min"` resolve to it.
- `"cp_p3phys"`/`"cp_p3func"` are added, unconditionally, to
  `map_comparam_name_service_timing`, resolving to `PARAM_P3_PHYS`/
  `PARAM_P3_FUNC` regardless of protocol.
- `map_comparam_name_for_protocol` is deleted; `GetComParam` now calls
  `map_comparam_name` (via the shared `resolve_comparam_name`, ADR-103)
  like every other name-resolution call site.
- No change to `comparam_support::check_param_allowed`/`is_can_param`/
  `is_kwp_param` — that layer was already correct.

This ADR does not supersede ADR-028: ADR-028 concerns
`to_j2534_config_id`'s per-protocol native-CONFIG-forwarding table, which
this fix does not touch.

## Consequences

- **Behavior change on an already-shipped RPC.** `GetComParam(name:
  "cp_p3phys")` on a KWP-family CLL previously succeeded, returning
  `P3_MIN`'s value; it now resolves `PARAM_P3_PHYS`
  and fails at `check_param_allowed` with the same rejection a numeric
  `PARAM_P3_PHYS` request on a KWP CLL already got. `CP_P3Min` is
  unaffected and continues to resolve to `P3_MIN` on every protocol
  (whether it is actually usable is, as before, decided by
  `check_param_allowed`, which restricts it to KWP-family).
- A client that was relying on the old aliasing to read/write P3 timing on
  KWP via the CAN-flavored names must switch to `CP_P3Min`, the name that
  was always the spec-correct one for that protocol.
- `names.rs`'s test suite: `map_comparam_name_for_protocol_resolves_p3_by_
  can_vs_kwp_family` is replaced by
  `map_comparam_name_resolves_p3_phys_func_independent_of_protocol`,
  asserting `CP_P3Phys`/`CP_P3Func`/`CP_P3Min` each resolve to one fixed id
  regardless of protocol; the alias-table and case-insensitivity tests
  covering `CP_P3Phys`/`CP_P3Func` are updated to expect `PARAM_P3_PHYS`/
  `PARAM_P3_FUNC` instead of `P3_MIN`.
